//! Shares real actor components between semantic and allocation tests.

use std::num::NonZeroUsize;

use super::*;

/// Owns real actor components without a runtime so tests can control every phase.
///
/// Fields correspond to `TransactionParts`; tests retain replies, close authority,
/// and harvest completion explicitly instead of duplicating transaction behaviour.
/// Reusing these parts after a caught panic isolates reply-phase checks. It does
/// not model Store recovery: a real actor closes and drops its state on panic.
pub(in crate::store) struct Host<S, A, C: Execution<S, A>> {
    pub(in crate::store) state: S,
    pub(in crate::store) driver: Box<C::Driver>,
    pub(in crate::store) dependencies: DependencyValues,
    pub(in crate::store) issues: crate::IssueReporter,
    pub(in crate::store) ingress: Arc<CommandIngress<Command<S, A, C>>>,
    pub(in crate::store) weak: Weak<CommandIngress<Command<S, A, C>>>,
    pub(in crate::store) receiver: crate::store::mailbox::Receiver<Command<S, A, C>>,
    pub(in crate::store) control: Arc<ControlWake>,
    pub(in crate::store) registry: TaskRegistry,
    pub(in crate::store) scopes: ScopeRegistry,
    pub(in crate::store) mounted: Vec<Box<C::Node>>,
    pub(in crate::store) mounts: Vec<Box<C::Mount>>,
    pub(in crate::store) handles: HandleRegistryCore<C::HandleSelector>,
    pub(in crate::store) version: u64,
    pub(in crate::store) task_groups: HashMap<TaskId, Arc<WorkGroup>>,
    pub(in crate::store) sequence_waits: HashMap<SequenceId, TaskId>,
    pub(in crate::store) sequences: HashMap<SequenceId, Sequence<DeferredQueue<S, A, C>>>,
    pub(in crate::store) next_sequence: SequenceId,
    pub(in crate::store) timers: TimerQueue<TimerPayload<S, A, C>>,
    pub(in crate::store) task_timers: HashMap<TaskId, TimerId>,
    pub(in crate::store) observations: ObservationRegistryCore<S, C::ObservationEntry>,
    pub(in crate::store) lifecycle: Arc<super::super::lifecycle::Lifecycle>,
}

impl<S: 'static, A: 'static, C: Execution<S, A>> Host<S, A, C> {
    /// Creates live root authority and a one-slot mailbox without installing work.
    pub(in crate::store) fn new(state: S, driver: Box<C::Driver>) -> Self {
        let (sender, receiver) = crate::store::mailbox::bounded(NonZeroUsize::new(1).unwrap());
        let control = ControlWake::new();
        let ingress = Arc::new(CommandIngress::new(sender, Arc::clone(&control)));
        let (lifecycle, scopes) = ScopeRegistry::for_test();
        Self {
            state,
            driver,
            dependencies: DependencyValues::new(),
            issues: crate::IssueReporter::default(),
            weak: Arc::downgrade(&ingress),
            ingress,
            receiver,
            registry: TaskRegistry::new(Arc::clone(&control)),
            control,
            scopes,
            mounted: Vec::new(),
            mounts: Vec::new(),
            handles: HandleRegistryCore::new(),
            version: 0,
            task_groups: HashMap::new(),
            sequence_waits: HashMap::new(),
            sequences: HashMap::new(),
            next_sequence: 0,
            timers: TimerQueue::new(),
            task_timers: HashMap::new(),
            observations: ObservationRegistryCore::new(),
            lifecycle,
        }
    }

    /// Borrows the production kernel at a fixed clock sample with no assertion gate.
    pub(in crate::store) fn parts(&mut self) -> TransactionParts<'_, S, A, C> {
        TransactionParts {
            #[cfg(feature = "diagnostics")]
            lifecycle_sink: None,
            state: &mut self.state,
            reducer: &mut *self.driver,
            dependencies: &mut self.dependencies,
            issues: &self.issues,
            ingress: &self.weak,
            registry: &mut self.registry,
            scopes: &mut self.scopes,
            mounted: &mut self.mounted,
            has_mount_recipes: self.mounts.iter().any(|mount| mount.can_discover()),
            mounts: &mut self.mounts,
            handles: &mut self.handles,
            control: &self.control,
            commit_version: &mut self.version,
            task_groups: &mut self.task_groups,
            sequence_waits: &mut self.sequence_waits,
            sequences: &mut self.sequences,
            next_sequence_id: &mut self.next_sequence,
            timers: &mut self.timers,
            task_timers: &mut self.task_timers,
            observations: &mut self.observations,
            now: MonotonicInstant::ZERO,
            #[cfg(feature = "test-util")]
            assertion_gate: &None,
            #[cfg(feature = "test-util")]
            structural_terminal: None,
        }
    }

    /// Commits directly and keeps the reply receiver for explicit phase assertions.
    pub(in crate::store) fn action(
        &mut self,
        action: A,
    ) -> (Turn<C::Future>, super::super::command::CommitReceiver) {
        let (reply, receive) = super::super::command::commit_channel();
        (
            commit(
                self.parts(),
                Mutation::Action(ActionCommand::external(action, reply), None),
            ),
            receive,
        )
    }

    /// Processes one admitted request without polling any returned runtime work.
    pub(in crate::store) fn next(&mut self) -> Turn<C::Future> {
        match self.receiver.try_recv().expect("one admitted command") {
            Command::Action(action) => commit(self.parts(), Mutation::Action(action, None)),
            Command::Mounted {
                call,
                provenance,
                authority,
                operation_work,
            } => {
                let turn = commit(self.parts(), Mutation::Mounted(call, provenance, authority));
                drop(operation_work);
                turn
            }
            Command::Task {
                request: call,
                provenance,
                authority,
                operation_work,
            } => {
                let turn = request(self.parts(), call, provenance, authority);
                drop(operation_work);
                turn
            }
            Command::Operation { request, metadata } => {
                super::operation(self.parts(), request, &*metadata)
            }
            _ => panic!("witness uses only action/read/update commands"),
        }
    }

    /// Harvests real terminal signals and returns any newly registered successors.
    pub(in crate::store) fn collect(&mut self) -> Launches<C::Future> {
        let terminal = self.registry.process_control().terminal;
        let mut launches = Launches::default();
        for task in terminal {
            self.parts().remove_task_timer(task.id);
            self.parts().complete_task(task, &mut launches);
        }
        launches
    }

    /// Captures typed root access without retaining the host's strong ingress owner.
    pub(in crate::store) fn context(&self) -> C::Context {
        C::context(
            Weak::clone(&self.weak),
            self.scopes.root(),
            Provenance::External { handle: None },
            self.scopes.authority(self.scopes.root()),
        )
    }
}

/// Supplies a minimal real reducer with no effect or observation side work.
pub(super) fn counter_host() -> Host<usize, usize, Shared> {
    Host::<usize, usize, Shared>::new(
        0,
        Box::new(crate::Reduce::new(
            |state: &mut usize, action: &usize, _: &mut ReduceContext<'_>| {
                *state += action;
                crate::Effect::none()
            },
        )),
    )
}
