//! Finite capacity-one admission and keyed-worker lifecycle sessions.
//!
//! Each session checks rejected payload identity, captured waits and replacement
//! isolation. Metadata stays alive through shutdown; state and task owners must not.

const INVALID: &str = "mixed pool work or ownership invalid";

#[derive(Clone, Copy, Eq, PartialEq)]
enum Fault {
    None,
    #[cfg(test)]
    SkipUnrelated,
    #[cfg(test)]
    RetainState,
    #[cfg(test)]
    RetainTask,
}

struct Config {
    name: String,
    local: bool,
    swap: bool,
    sessions: u64,
}
impl Config {
    fn parse(mut args: impl Iterator<Item = String>) -> Result<Self, &'static str> {
        let name = args.next().ok_or("missing mixed pool workload")?;
        let (local, swap) = match name.as_str() {
            "mixed-pool-shared-ordered" => (false, false),
            "mixed-pool-shared-swap" => (false, true),
            "mixed-pool-local-ordered" => (true, false),
            "mixed-pool-local-swap" => (true, true),
            _ => return Err("unknown mixed pool workload"),
        };
        if args.next().as_deref() != Some("1000") {
            return Err("mixed pool requires SIZE=1000");
        }
        let sessions = match args.next().as_deref() {
            Some("1") => 1,
            Some("64") => 64,
            _ => return Err("mixed pool requires ITERATIONS=1 or 64"),
        };
        if args.next().is_some() {
            return Err("unexpected mixed pool argument");
        }
        Ok(Self {
            name,
            local,
            swap,
            sessions,
        })
    }
}

pub(crate) fn run(args: impl Iterator<Item = String>) -> Result<String, &'static str> {
    run_fault(Config::parse(args)?, Fault::None)
}

fn run_fault(config: Config, fault: Fault) -> Result<String, &'static str> {
    if config.local {
        #[cfg(feature = "local")]
        return local::profile(config, fault);
        #[cfg(not(feature = "local"))]
        return Err("mixed Local pool requires local feature");
    }
    shared::profile(config, fault)
}

#[cfg(test)]
fn run_control(fault: &str) -> Result<String, &'static str> {
    let fault = match fault {
        "skip-unrelated" => Fault::SkipUnrelated,
        "retain-state" => Fault::RetainState,
        "retain-task" => Fault::RetainTask,
        _ => return Err("unknown mixed pool control"),
    };
    run_fault(
        Config {
            name: "mixed-pool-shared-ordered".into(),
            local: false,
            swap: false,
            sessions: 1,
        },
        fault,
    )
}

#[derive(Clone, Copy, Default, Debug, Eq, PartialEq)]
struct Counts {
    accepted: u64,
    full: u64,
    pending_outputs: u64,
    outputs: u64,
    captured: u64,
    completed: u64,
    cancelled: u64,
    waits: u64,
    later_pending: u64,
    later_cancelled: u64,
    structural: u64,
    breaks: u64,
    folds: u64,
    updates: u64,
    stale: u64,
    errors: u64,
}

// Independent literal oracles from the approved one/64-session schedule.
fn validate(counts: Counts, sessions: u64, sum: u64) -> Result<(), &'static str> {
    let (expected, wanted_sum) = match sessions {
        1 => (
            Counts {
                accepted: 4,
                full: 1,
                pending_outputs: 1,
                outputs: 2,
                captured: 2,
                completed: 1,
                cancelled: 1,
                waits: 2,
                later_pending: 2,
                later_cancelled: 2,
                structural: 2,
                breaks: 1,
                folds: 1000,
                updates: 1,
                stale: 2,
                errors: 0,
            },
            20,
        ),
        64 => (
            Counts {
                accepted: 256,
                full: 64,
                pending_outputs: 64,
                outputs: 128,
                captured: 128,
                completed: 64,
                cancelled: 64,
                waits: 128,
                later_pending: 128,
                later_cancelled: 128,
                structural: 128,
                breaks: 64,
                folds: 64000,
                updates: 64,
                stale: 128,
                errors: 0,
            },
            587,
        ),
        _ => return Err(INVALID),
    };
    if counts != expected || sum != wanted_sum {
        return Err(INVALID);
    }
    Ok(())
}

// Two concrete ownership domains, with no type-erased or generic host layer.
macro_rules! pool_fixture {
    () => {

        use super::{Config, Counts, Fault, INVALID, validate};
        use crate::measurement::Measurement;
        use rca_core::{
            FoldKey, KeyedState, OperationPolicy, OutputDelivery, OutputFlow, TaskAccessReason,
            TaskProgress, TaskSendError
        };
        use std::future::Future;
        use std::ops::ControlFlow;
        use std::pin::Pin;
        use std::sync::Mutex;
        use std::task::{Context, Poll, Waker};

        const WIDTH: usize = 1000;
        #[derive(Default)]
        struct Probes {
            counts: Mutex<Counts>,
            state_created: Mutex<u64>,
            state_dropped: Mutex<u64>,
            task_created: Mutex<u64>,
            task_dropped: Mutex<u64>,
            states: Mutex<Vec<Weak<()>>>,
            tasks: Mutex<Vec<Weak<Payload>>>,
        }

        struct State {
            value: u64,
            probes: Owner<Probes>,
            _owner: Owner<()>
        }

        impl State {
            fn new(probes: &Owner<Probes>) -> Self {
                let owner = Owner::new(());
                probes.states.lock().unwrap().push(Owner::downgrade(&owner));
                *probes.state_created.lock().unwrap() += 1;
                Self {
                    value: 0,
                    probes: Owner::clone(probes),
                    _owner: owner
                }
            }
        }

        impl Drop for State {
            fn drop(&mut self) {
                *self.probes.state_dropped.lock().unwrap() += 1;
            }
        }
        // Concrete owned task data; only its weak witness survives normal cleanup.
        struct Payload {
            release: async_channel::Receiver<()>,
            probes: Owner<Probes>,
            bytes: Box<[u8; 1024]>,
        }

        impl Drop for Payload {
            fn drop(&mut self) {
                *self.probes.task_dropped.lock().unwrap() += 1;
            }
        }

        struct Work {
            amount: Box<u64>,
            payload: Option<Owner<Payload>>
        }

        fn work(amount: u64, probes: &Owner<Probes>) -> (Work, async_channel::Sender<()>) {
            let (release, receive) = async_channel::bounded(1);
            let payload = Owner::new(Payload {
                release: receive,
                probes: Owner::clone(probes),
                bytes: Box::new([0x5a; 1024])
            });
            *probes.task_created.lock().unwrap() += 1;
            probes
                .tasks
                .lock()
                .unwrap()
                .push(Owner::downgrade(&payload));
            (
                Work {
                    amount: Box::new(amount),
                    payload: Some(payload)
                },
                release
            )
        }

        struct Worker {
            output: Owner<Mutex<Option<Output<()>>>>
        }

        impl Feature for Worker {
            type State = State;
            type Action = Work;
            fn update(
                &mut self,
                state: &mut State,
                action: &Work,
                context: &mut FeatureContext<'_, State, Work>
            ) {
                state.value += *action.amount;
                state.probes.counts.lock().unwrap().accepted += 1;
                let payload = Owner::clone(action.payload.as_ref().expect("accepted work has payload"));
                let output = self
                    .output
                    .lock()
                    .unwrap()
                    .as_ref()
                    .expect("selected worker output")
                    .clone();
                context.task(Task::<State, Work>::run_named(
                    "work",
                    OperationPolicy::Coexist,
                    move |context| async move {
                        if payload.release.recv().await.is_err() {
                            payload.probes.counts.lock().unwrap().errors += 1;
                            return;
                        }
                        if payload.bytes.iter().any(|byte| *byte != 0x5a) {
                            payload.probes.counts.lock().unwrap().errors += 1;
                        }
                        let probes = Owner::clone(&payload.probes);
                        if context
                            .update(move |state| {
                                state.value += 100;
                                probes.counts.lock().unwrap().updates += 1;
                            })
                            .await
                            .is_err()
                        {
                            payload.probes.counts.lock().unwrap().errors += 1;
                            return;
                        }
                        match context.output(&output, ()).await {
                            Ok(OutputDelivery::Consumed { .. }) => {},
                            _ => payload.probes.counts.lock().unwrap().errors += 1,
                        }
                    }
                ));
            }
        }

        struct Root {
            rows: KeyedState<usize, State>,
            outputs: u64
        }

        enum Action {
            Remove,
            Insert
        }

        struct Pool {
            probes: Owner<Probes>,
            swap: bool
        }

        impl Feature for Pool {
            type State = Root;
            type Action = Action;
            fn update(
                &mut self,
                root: &mut Root,
                action: &Action,
                _: &mut FeatureContext<'_, Root, Action>
            ) {
                match action {
                    Action::Remove => {
                        let removed = if self.swap {
                            root.rows.swap_remove(&0)
                        } else {
                            root.rows.remove(&0)
                        };
                        if removed.is_none() {
                            self.probes.counts.lock().unwrap().errors += 1;
                        }
                    },
                    Action::Insert => root.rows.insert(0, State::new(&self.probes)),
                }
                self.probes.counts.lock().unwrap().structural += 1;
            }
        }

        struct Sum;
        impl FoldKey for Sum {
            type Value = u64;
            fn identity() -> u64 {
                0
            }
            fn reduce(acc: u64, next: u64) -> ControlFlow<u64, u64> {
                ControlFlow::Continue(acc + next)
            }
        }

        struct First;
        impl FoldKey for First {
            type Value = usize;
            fn identity() -> usize {
                usize::MAX
            }
            fn reduce(_: usize, next: usize) -> ControlFlow<usize, usize> {
                ControlFlow::Break(next)
            }
        }

        #[derive(Clone)]
        struct Routes {
            site: MountSite<Root, State, Work, Action>,
            command: CommandSite<Root, Work>,
            output: Output<()>,
        }

        fn poll<F: Future + ?Sized>(future: Pin<&mut F>) -> Poll<F::Output> {
            future.poll(&mut Context::from_waker(Waker::noop()))
        }
        macro_rules! drive {
            ($host:expr, $future:expr) => {
                crate::finite_drive::finish($host.run($future))?
                    .map_err(|_| "mixed pool deterministic drive failed")?
            };
        }

        /// Declares the cold graph inside the construction measurement.
        fn build_store(
            host: &DeterministicRuntime,
            probes: &Owner<Probes>,
            routes: &Owner<Mutex<[Option<Routes>; 2]>>,
            first: &Fold<Root, First>,
            sum: &Fold<Root, Sum>,
            swap: bool,
        ) -> Result<Store<Root, Action>, &'static str> {
            let declared_first = first.clone();
            let declared_sum = sum.clone();
            let declared_routes = Owner::clone(routes);
            let declared_probes = Owner::clone(probes);
            let root = Root {
                rows: (0..WIDTH).map(|id| (id, State::new(probes))).collect(),
                outputs: 0
            };
            Store::builder(
                root,
                Pool {
                    probes: Owner::clone(probes),
                    swap
                },
                host.runtime()
            )
            .mailbox_capacity(1)
            .keyed_with_children(
                |root| &root.rows,
                |root| &mut root.rows,
                move |id, _| {
                    let id = *id;
                    let first = declared_first.clone();
                    let sum = declared_sum.clone();
                    let routes = Owner::clone(&declared_routes);
                    let probes = Owner::clone(&declared_probes);
                    let output = Owner::new(Mutex::new(None));
                    let worker = Worker {
                        output: Owner::clone(&output)
                    };
                    (
                        EntryMount::new(worker),
                        move |_: &State, children: &mut MountChildren<'_, Root, Action, State, Work>| {
                            let selected = Owner::clone(&probes);
                            children.contribute(&first, move |_| {
                                selected.counts.lock().unwrap().breaks += 1;
                                id
                            });
                            let selected = Owner::clone(&probes);
                            children.contribute(&sum, move |state| {
                                selected.counts.lock().unwrap().folds += 1;
                                state.value
                            });
                            if id < 2 {
                                let site = children.site().clone();
                                let command = children.command(|work: Work| work);
                                let selected = Owner::clone(&probes);
                                let channel = children.output(&site, |handlers| {
                                    handlers.at_root(move |root, (), _| {
                                        root.outputs += 1;
                                        selected.counts.lock().unwrap().outputs += 1;
                                        OutputFlow::Consumed
                                    });
                                });
                                *output.lock().unwrap() = Some(channel.clone());
                                routes.lock().unwrap()[id] = Some(Routes {
                                    site,
                                    command,
                                    output: channel
                                });
                            }
                        }
                    )
                }
            )
            .build()
            .map_err(|_| "mixed pool Store build failed")
        }

        #[expect(
            clippy::too_many_lines,
            reason = "Keeps admission, captured waits and retained handles in one measured lifetime through teardown."
        )]
        pub(super) fn profile(config: Config, fault: Fault) -> Result<String, &'static str> {
            #[cfg(not(test))]
            let _ = fault;
            let mut measurement = Measurement::new(None);
            let host = DeterministicRuntime::with_step_budget(100_000);
            let probes = Owner::new(Probes::default());
            let routes = Owner::new(Mutex::new([None::<Routes>, None]));
            let first = Fold::<Root, First>::new();
            let sum = Fold::<Root, Sum>::new();
            let store = build_store(&host, &probes, &routes, &first, &sum, config.swap)?;
            measurement.construction_finished();

            drive!(host, store.ready()).map_err(|_| INVALID)?;
            host.run_until_stalled().map_err(|_| INVALID)?;
            measurement.cold_finished();

            let mut expected_ids: Vec<_> = (0..WIDTH).collect();
            let mut metadata = Vec::new();
            let mut task_handles = Vec::new();
            let mut named_waits = Vec::new();
            let mut final_sum = 0;
            let mut first_id = usize::MAX;
            let mut sessions_drained = true;
            #[cfg(test)]
            let mut retained_payload = None;
            for session in 1..=config.sessions {
                let [a, b] = routes.lock().unwrap().clone();
                let a = a.ok_or(INVALID)?;
                let b = b.ok_or(INVALID)?;
                let a_context = drive!(host, store.acquire(&a.site))
                    .map_err(|_| INVALID)?
                    .ok_or(INVALID)?;
                let b_context = drive!(host, store.acquire(&b.site))
                    .map_err(|_| INVALID)?
                    .ok_or(INVALID)?;
                let a_port = drive!(host, store.command_port(&a.command))
                    .map_err(|_| INVALID)?
                    .ok_or(INVALID)?;
                let b_port = drive!(host, store.command_port(&b.command))
                    .map_err(|_| INVALID)?
                    .ok_or(INVALID)?;

                let (command, release_a) = work(1, &probes);
                #[cfg(test)]
                if fault == Fault::RetainTask {
                    retained_payload = command.payload.clone();
                }
                let admitted_a = a_port.try_send(command).map_err(|_| INVALID)?;
                let mut output = Box::pin(a_context.output(&a.output, ()));
                if !poll(output.as_mut()).is_pending() {
                    return Err(INVALID);
                }
                probes.counts.lock().unwrap().pending_outputs += 1;

                let (command, release_b) = work(2, &probes);
                let identity = &raw const *command.amount;
                let Err(TryCommandError::Full(command)) = b_port.try_send(command) else {
                    return Err(INVALID);
                };
                if (&raw const *command.amount) != identity || *command.amount != 2 {
                    return Err(INVALID);
                }
                probes.counts.lock().unwrap().full += 1;

                let task_a = drive!(host, admitted_a.committed()).map_err(|_| INVALID)?;
                let delivered = drive!(host, output).map_err(|_| INVALID)?;
                let OutputDelivery::Consumed { task: output_task } = delivered else {
                    return Err(INVALID);
                };
                drive!(host, output_task.finish());
                let admitted_b = b_port.try_send(command).map_err(|_| INVALID)?;
                let task_b = drive!(host, admitted_b.committed()).map_err(|_| INVALID)?;
                host.run_until_stalled().map_err(|_| INVALID)?;

                let a_operation = a_context.operation("work");
                let b_operation = b_context.operation("work");
                // Capture is an actor request; later work cannot join these waits.
                let mut wait_a = Box::pin(drive!(host, a_operation.capture()).map_err(|_| INVALID)?);
                let mut wait_b = Box::pin(drive!(host, b_operation.capture()).map_err(|_| INVALID)?);
                for task in [&task_a, &task_b] {
                    if task.is_finished() || task.is_cancelled() {
                        return Err(INVALID);
                    }
                    let mut finish = Box::pin(task.finish());
                    if !poll(finish.as_mut()).is_pending() {
                        return Err(INVALID);
                    }
                    probes.counts.lock().unwrap().captured += 1;
                }
                if !poll(wait_a.as_mut()).is_pending() || !poll(wait_b.as_mut()).is_pending() {
                    return Err(INVALID);
                }

                release_a.try_send(()).map_err(|_| INVALID)?;
                drive!(host, task_a.finish());
                drive!(host, wait_a.as_mut()).map_err(|_| INVALID)?;
                if task_a.is_cancelled() || !task_a.is_finished() || task_b.is_finished() {
                    return Err(INVALID);
                }
                probes.counts.lock().unwrap().completed += 1;
                probes.counts.lock().unwrap().waits += 1;
                if !poll(wait_b.as_mut()).is_pending() {
                    return Err(INVALID);
                }
                let value = drive!(host, a_context.read(|state| state.value)).map_err(|_| INVALID)?;
                if value != if session == 1 { 101 } else { 112 } {
                    return Err(INVALID);
                }
                #[cfg(test)]
                let skip = fault == Fault::SkipUnrelated;
                #[cfg(not(test))]
                let skip = false;
                let unrelated = if skip {
                    None
                } else {
                    let (command, release) = work(7, &probes);
                    Some((
                        drive!(host, b_port.send(command)).map_err(|_| INVALID)?,
                        release
                    ))
                };

                let removed = drive!(host, store.send(Action::Remove)).map_err(|_| INVALID)?;
                let position = expected_ids.iter().position(|id| *id == 0).ok_or(INVALID)?;
                if config.swap {
                    expected_ids.swap_remove(position);
                } else {
                    expected_ids.remove(position);
                }
                let ids = drive!(
                    host,
                    store.read(|root| root.rows.iter().map(|(id, _)| *id).collect::<Vec<_>>())
                )
                .map_err(|_| INVALID)?;
                if ids != expected_ids {
                    return Err(INVALID);
                }

                let inserted = drive!(host, store.send(Action::Insert)).map_err(|_| INVALID)?;
                expected_ids.push(0);
                let ids = drive!(
                    host,
                    store.read(|root| root.rows.iter().map(|(id, _)| *id).collect::<Vec<_>>())
                )
                .map_err(|_| INVALID)?;
                if ids != expected_ids {
                    return Err(INVALID);
                }

                let replacement = routes.lock().unwrap()[0].clone().ok_or(INVALID)?;
                let replacement_context = drive!(host, store.acquire(&replacement.site))
                    .map_err(|_| INVALID)?
                    .ok_or(INVALID)?;
                let replacement_port = drive!(host, store.command_port(&replacement.command))
                    .map_err(|_| INVALID)?
                    .ok_or(INVALID)?;
                let (command, release_replacement) = work(11, &probes);
                let replacement_task =
                    drive!(host, replacement_port.send(command)).map_err(|_| INVALID)?;

                let stale_command = Work {
                    amount: Box::new(99),
                    payload: None
                };
                // Admission is not commit: stale authority is rejected by the actor.
                let stale_receipt = a_port.try_send(stale_command).map_err(|_| INVALID)?;
                let Err(TaskSendError::Rejected {
                    action: command,
                    error
                }) = drive!(host, stale_receipt.committed())
                else {
                    return Err(INVALID);
                };
                if *command.amount != 99
                    || error.reason() != TaskAccessReason::ScopeEnded
                    || error.progress() != TaskProgress::NotStarted
                {
                    return Err(INVALID);
                }
                probes.counts.lock().unwrap().stale += 1;
                let error = drive!(host, a_context.read(|_| ())).err().ok_or(INVALID)?;
                if error.reason() != TaskAccessReason::ScopeEnded
                    || error.progress() != TaskProgress::NotStarted
                {
                    return Err(INVALID);
                }
                probes.counts.lock().unwrap().stale += 1;

                task_b.cancel();
                if !task_b.is_cancelled() {
                    return Err(INVALID);
                }
                drive!(host, task_b.finish());
                drive!(host, wait_b.as_mut()).map_err(|_| INVALID)?;
                if !task_b.is_finished() {
                    return Err(INVALID);
                }
                probes.counts.lock().unwrap().cancelled += 1;
                probes.counts.lock().unwrap().waits += 1;
                host.run_until_stalled().map_err(|_| INVALID)?;
                for task in unrelated
                    .iter()
                    .map(|(task, _)| task)
                    .chain([&replacement_task])
                {
                    if task.is_finished() || task.is_cancelled() {
                        return Err(INVALID);
                    }
                    let mut finish = Box::pin(task.finish());
                    if !poll(finish.as_mut()).is_pending() {
                        return Err(INVALID);
                    }
                    probes.counts.lock().unwrap().later_pending += 1;
                }

                first_id = drive!(host, store.fold(&first)).map_err(|_| INVALID)?;
                final_sum = drive!(host, store.fold(&sum)).map_err(|_| INVALID)?;
                if first_id != if config.swap { 999 } else { 1 } {
                    return Err(INVALID);
                }
                for task in unrelated
                    .iter()
                    .map(|(task, _)| task)
                    .chain([&replacement_task])
                {
                    task.cancel();
                    drive!(host, task.finish());
                    if !task.is_cancelled() || !task.is_finished() {
                        return Err(INVALID);
                    }
                    probes.counts.lock().unwrap().later_cancelled += 1;
                }

                drive!(host, removed.finish());
                drive!(host, inserted.finish());
                host.run_until_stalled().map_err(|_| INVALID)?;
                sessions_drained &= *probes.task_dropped.lock().unwrap() == 4 * session
                    && probes
                        .tasks
                        .lock()
                        .unwrap()
                        .iter()
                        .all(|owner| owner.upgrade().is_none());

                task_handles.extend([
                    task_a,
                    task_b,
                    replacement_task,
                    removed,
                    inserted,
                    output_task
                ]);
                if let Some((task, release)) = unrelated {
                    task_handles.push(task);
                    drop(release);
                }
                named_waits.extend([wait_a, wait_b]);
                metadata.extend([
                    (a_port, a_context, a_operation),
                    (b_port, b_context, b_operation),
                    (
                        replacement_port,
                        replacement_context.clone(),
                        replacement_context.operation("work")
                    )
                ]);
                drop((release_a, release_b, release_replacement));
            }
            measurement.work_finished();

            let counts = *probes.counts.lock().unwrap();
            let outputs = drive!(host, store.read(|root| root.outputs)).map_err(|_| INVALID)?;
            let work_valid = validate(counts, config.sessions, final_sum).is_ok()
                && outputs == counts.outputs
                && sessions_drained;
            measurement.validation_finished();

            #[cfg(test)]
            let retained_state = if fault == Fault::RetainState {
                Some(drive!(host, store.clone().shutdown_into_state()).map_err(|_| INVALID)?)
            } else {
                drive!(host, store.shutdown()).map_err(|_| INVALID)?;
                None
            };
            #[cfg(not(test))]
            drive!(host, store.shutdown()).map_err(|_| INVALID)?;
            host.run_until_stalled().map_err(|_| INVALID)?;
            host.assert_no_pending_time().map_err(|_| INVALID)?;
            let state_created = *probes.state_created.lock().unwrap();
            let state_dropped = *probes.state_dropped.lock().unwrap();
            let task_created = *probes.task_created.lock().unwrap();
            let task_dropped = *probes.task_dropped.lock().unwrap();
            let alive = probes
                .states
                .lock()
                .unwrap()
                .iter()
                .filter(|owner| owner.upgrade().is_some())
                .count()
                + probes
                    .tasks
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|owner| owner.upgrade().is_some())
                    .count();
            let (wanted_states, wanted_tasks) = if config.sessions == 1 {
                (1001, 4)
            } else {
                (1064, 256)
            };
            let owners_valid = state_created == wanted_states
                && state_dropped == wanted_states
                && task_created == wanted_tasks
                && task_dropped == wanted_tasks
                && alive == 0;
            #[cfg(test)]
            drop((retained_state, retained_payload));
            // Weak routes, captured waits and receipts are deliberately held through teardown.
            drop((
                metadata,
                task_handles,
                named_waits,
                expected_ids,
                routes,
                first,
                sum,
                store,
                host
            ));
            if !work_valid || !owners_valid || Owner::strong_count(&probes) != 1 {
                return Err(INVALID);
            }
            drop(probes);
            let allocations = measurement
                .finish()?
                .map_or_else(|| "null".to_owned(), |record| record.json());
            Ok(format!(
                concat!(
                    "{{\"schema\":\"rca-performance-profile-v2\",\"workload\":\"{}\",\"domain\":\"{}\",",
                    "\"backend\":\"deterministic\",\"depth\":1,\"width\":1000,\"capacity\":1,\"removal\":\"{}\",\"iterations\":{},",
                    "\"sum\":{},\"first_id\":{},\"useful_work\":{{\"accepted_commands\":{},\"full_rejections\":{},\"pending_outputs\":{},",
                    "\"outputs\":{},\"captured_tasks\":{},\"captured_completions\":{},\"captured_cancellations\":{},\"captured_waits_finished\":{},",
                    "\"later_tasks_pending\":{},\"later_tasks_cancelled\":{},\"structural_commits\":{},\"break_callbacks\":{},\"full_callbacks\":{}}},",
                    "\"ownership\":{{\"state_created\":{},\"state_dropped\":{},\"task_created\":{},\"task_dropped\":{},\"alive\":{}}},\"allocations\":{}}}"
                ),
                config.name,
                if config.local {
                    "Local-Rc"
                } else {
                    "Shared-Arc"
                },
                if config.swap { "swap" } else { "ordered" },
                config.sessions,
                final_sum,
                first_id,
                counts.accepted,
                counts.full,
                counts.pending_outputs,
                counts.outputs,
                counts.captured,
                counts.completed,
                counts.cancelled,
                counts.waits,
                counts.later_pending,
                counts.later_cancelled,
                counts.structural,
                counts.breaks,
                counts.folds,
                state_created,
                state_dropped,
                task_created,
                task_dropped,
                alive,
                allocations
            ))
        }
    };
}

mod shared {
    use rca_core::{
        CommandSite, EntryMount, Feature, FeatureContext, Fold, MountChildren, MountSite, Output,
        Store, Task, TryCommandError,
    };
    use rca_test::DeterministicRuntime;
    use std::sync::{Arc as Owner, Weak};
    pool_fixture!();
}

#[cfg(feature = "local")]
mod local {
    use rca_core::local::{
        CommandSite, EntryMount, Feature, FeatureContext, Fold, MountChildren, MountSite, Output,
        Store, Task, TryCommandError,
    };
    use rca_test::local::DeterministicRuntime;
    use std::rc::{Rc as Owner, Weak};
    pool_fixture!();
}

#[cfg(test)]
mod tests {
    use super::{run, run_control};

    #[test]
    fn capacity_one_pool_completes_only_the_captured_work() {
        #[cfg(feature = "allocations")]
        if !crate::tests::in_child(
            "mixed_pool::tests::capacity_one_pool_completes_only_the_captured_work",
        ) {
            return;
        }
        for domain in ["shared", "local"] {
            for removal in ["ordered", "swap"] {
                for (iterations, sum) in [(1, 20), (64, 587)] {
                    let record = run([
                        format!("mixed-pool-{domain}-{removal}"),
                        "1000".into(),
                        iterations.to_string(),
                    ]
                    .into_iter())
                    .unwrap();
                    let record: serde_json::Value = serde_json::from_str(&record).unwrap();
                    assert_eq!(record["depth"], 1);
                    assert_eq!(record["width"], 1000);
                    assert_eq!(record["capacity"], 1);
                    assert_eq!(record["removal"], removal);
                    assert_eq!(record["useful_work"]["accepted_commands"], iterations * 4);
                    assert_eq!(record["useful_work"]["full_rejections"], iterations);
                    assert_eq!(record["useful_work"]["pending_outputs"], iterations);
                    assert_eq!(record["useful_work"]["outputs"], iterations * 2);
                    assert_eq!(record["useful_work"]["captured_tasks"], iterations * 2);
                    assert_eq!(record["useful_work"]["captured_completions"], iterations);
                    assert_eq!(record["useful_work"]["captured_cancellations"], iterations);
                    assert_eq!(
                        record["useful_work"]["captured_waits_finished"],
                        iterations * 2
                    );
                    assert_eq!(record["useful_work"]["later_tasks_pending"], iterations * 2);
                    assert_eq!(
                        record["useful_work"]["later_tasks_cancelled"],
                        iterations * 2
                    );
                    assert_eq!(record["useful_work"]["structural_commits"], iterations * 2);
                    assert_eq!(record["useful_work"]["break_callbacks"], iterations);
                    assert_eq!(record["useful_work"]["full_callbacks"], iterations * 1000);
                    assert_eq!(record["sum"], sum);
                    assert_eq!(
                        record["first_id"],
                        if removal == "ordered" { 1 } else { 999 }
                    );
                    assert_eq!(record["ownership"]["task_created"], iterations * 4);
                    assert_eq!(record["ownership"]["task_dropped"], iterations * 4);
                    assert_eq!(record["ownership"]["state_created"], 1000 + iterations);
                    assert_eq!(record["ownership"]["state_dropped"], 1000 + iterations);
                    assert_eq!(record["ownership"]["alive"], 0);
                }
            }
        }
    }

    #[test]
    fn rejects_skipped_admission_and_retained_concrete_work() {
        #[cfg(feature = "allocations")]
        if !crate::tests::in_child(
            "mixed_pool::tests::rejects_skipped_admission_and_retained_concrete_work",
        ) {
            return;
        }
        for fault in ["skip-unrelated", "retain-state", "retain-task"] {
            assert_eq!(
                run_control(fault).err(),
                Some("mixed pool work or ownership invalid")
            );
        }
    }
}
