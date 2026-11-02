//! Owns effect task registration, cancellation and terminal harvesting.
//!
//! The actor alone mutates registry membership. Runtime futures share only the
//! cancellation and completion signals. Preparing and registering a task form
//! one actor phase: control processing cannot run between them. Runtime exit
//! publishes a terminal status; harvesting then releases the task's causal work.
//! Named operation leases also cover queued commands and deferred work, so a
//! terminal task does not by itself prove that its named operation has finished.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crossbeam_utils::atomic::AtomicCell;
use futures_util::future::{AbortHandle, AbortRegistration, Abortable, FutureExt};

use super::CancellationIdentity;
use super::lineage::{ControlWake, LineageState, LineageWork};
use super::operation::{
    ManagedPoll, OperationAncestry, OperationStatus, OperationWait, OperationWork,
};
use crate::RuntimeFuture;
use crate::store::scope::{ScopeToken, ScopeTransitionBatch};

#[cfg(test)]
thread_local! {
    // Counts record visits, not hash-table bucket probes or elapsed time.
    pub(crate) static OPERATION_RECORD_VISITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Identifies one cancellation generation without retaining a registry holder.
///
/// Clones retain the shared liveness signal. Deferred compiler frames separately
/// balance [`TaskRegistry::retain_generation`] and [`TaskRegistry::release_generation`].
#[derive(Clone, Debug)]
pub(crate) struct GenerationToken {
    /// User key and structural scope that select the cancellation entry.
    identity: CancellationIdentity,
    /// Monotone identifier within the issuing registry.
    generation: u64,
    /// Shared revocation flag; pointer identity also rejects foreign tokens.
    live: Arc<AtomicBool>,
    /// Named operation completion, absent for ordinary cancellation generations.
    completion: Option<Arc<LineageState>>,
}

impl GenerationToken {
    /// Checks registry revocation and cancellation requested through a named wait.
    pub(crate) fn is_live(&self) -> bool {
        self.live.load(Ordering::Acquire)
            && self
                .completion
                .as_ref()
                .is_none_or(|completion| !completion.is_cancelled())
    }
}

/// Keeps a generation reachable while its compiler holders or work remain.
struct GenerationRecord {
    /// Revocation signal shared with every token for this exact generation.
    live: Arc<AtomicBool>,
    /// Reserved and registered task IDs, detached during terminal harvesting.
    tasks: HashSet<u64>,
    /// Explicit compiler-frame holds, independent of token clone counts.
    holders: usize,
    /// Named completion keeps the record alive beyond its registered tasks.
    completion: Option<Arc<LineageState>>,
}

/// Groups concurrent generations of one cancellation identity.
struct CancellationEntry {
    /// Stable identity for test snapshots without formatting the user's key.
    #[cfg(feature = "test-util")]
    diagnostic_id: u64,
    /// Generation IDs are unique within this registry, including replacements.
    active: HashMap<u64, GenerationRecord>,
}

/// Records the first terminal signal published for a registered task.
///
/// Harvesting can still promote `Completed` to `Cancelled` when an abort races
/// with the final poll. Store teardown can publish cancellation before runtime exit.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TaskStatus {
    /// No terminal signal has won the completion transition.
    Running = 0,
    /// The actor or runtime finished the task normally.
    Completed = 1,
    /// Cancellation ended the task or invalidated its owner during teardown.
    Cancelled = 2,
    /// The runtime wrapper was destroyed while its thread was unwinding.
    Panicked = 3,
    /// The runtime dropped an unfinished wrapper outside an unwind.
    RuntimeDropped = 4,
}

/// Shares a monotone task status and wakes the actor on its first transition.
#[derive(Debug)]
pub(crate) struct TaskCompletion {
    /// Only the first transition away from `Running` succeeds.
    status: AtomicCell<TaskStatus>,
    /// Actor notification is separate from causal completion notification.
    control: Arc<ControlWake>,
}

impl TaskCompletion {
    /// Creates the running signal before registration transfers work ownership.
    fn new(control: Arc<ControlWake>) -> Arc<Self> {
        Arc::new(Self {
            status: AtomicCell::new(TaskStatus::Running),
            control,
        })
    }

    /// Checks the task-status part of effect-action authority.
    pub(crate) fn is_running(&self) -> bool {
        self.status.load() == TaskStatus::Running
    }

    /// Reads the runtime signal before the actor applies harvest-time cancellation.
    fn status(&self) -> TaskStatus {
        self.status.load()
    }

    /// Publishes a terminal status once and reports whether this call won.
    ///
    /// Callers supply a non-running status. A lost transition never wakes the actor.
    fn finish(&self, status: TaskStatus) -> bool {
        if self
            .status
            .compare_exchange(TaskStatus::Running, status)
            .is_err()
        {
            return false;
        }
        self.control.notify();
        true
    }
}

/// Publishes runtime completion even if its factory or future unwinds.
///
/// The wrapper drops the concrete future before this guard. Normal completion
/// releases named work before publishing status; on unwind, field drop glue
/// releases the work after the guard has published the unwind status.
struct CompletionGuard<W = OperationWork> {
    /// Signal retained independently of the actor's task node.
    completion: Arc<TaskCompletion>,
    /// Prevents normal completion from being replaced by a drop status.
    terminal: bool,
    /// Named work is owned here, not by an already-completed runtime future.
    operations: Option<W>,
}

impl<W> CompletionGuard<W> {
    /// Takes the work released after the concrete input is destroyed.
    fn new(completion: Arc<TaskCompletion>, operations: W) -> Self {
        Self {
            completion,
            terminal: false,
            operations: Some(operations),
        }
    }

    /// Releases named work before publishing normal or aborted runtime completion.
    fn finish(&mut self, status: TaskStatus) {
        drop(self.operations.take());
        self.completion.finish(status);
        self.terminal = true;
    }
}

impl<W> Drop for CompletionGuard<W> {
    fn drop(&mut self) {
        if !self.terminal {
            let status = if std::thread::panicking() {
                TaskStatus::Panicked
            } else {
                TaskStatus::RuntimeDropped
            };
            self.completion.finish(status);
        }
    }
}

/// Wraps a concrete future without boxing it separately from its completion guard.
///
/// `Map` destroys its inner `Abortable` before invoking the terminal callback and
/// drops that inner field before the callback on abandonment. Both paths keep
/// captured payload destruction ahead of completion publication.
fn track_completion<W>(
    future: impl Future<Output = ()>,
    registration: AbortRegistration,
    mut guard: CompletionGuard<W>,
) -> impl Future<Output = ()> {
    Abortable::new(future, registration).map(move |result| {
        let status = if result.is_ok() {
            TaskStatus::Completed
        } else {
            TaskStatus::Cancelled
        };
        guard.finish(status);
    })
}

/// Retains actor bookkeeping and causal work until terminal harvesting.
struct TaskNode {
    /// Exact structural incarnation used by descendant cancellation.
    scope: ScopeToken,
    /// Causal cancellation domain shared with the initiating action.
    lineage: Arc<LineageState>,
    /// Runtime or actor terminal signal; observing it does not remove the node.
    completion: Arc<TaskCompletion>,
    /// Selects who can finish the task and how cancellation reaches it.
    authority: TaskAuthority,
    /// Immutable memberships removed before this node releases causal work.
    generations: Box<[GenerationToken]>,
    /// Last field: harvesting records the outcome before dropping this work lease.
    _work: LineageWork,
}

/// Distinguishes runtime cancellation from synchronously owned actor timers.
enum TaskAuthority {
    /// Requests abort; the runtime wrapper normally acknowledges actual exit.
    Runtime(AbortHandle),
    /// The actor destroys the timer payload and publishes its terminal state.
    Actor,
}

/// Monotone task identity local to one registry.
pub(crate) type TaskId = u64;

/// Returns a harvested outcome to the actor's timer and work-group bookkeeping.
#[derive(Clone, Copy)]
pub(crate) struct TerminalTask {
    /// Identity of the node already removed from the registry.
    pub(crate) id: TaskId,
    /// Final disposition after resolving a concurrent cancellation request.
    pub(crate) status: TaskStatus,
}

/// Keeps zero or one harvested outcomes inline on the ready-effect path.
///
/// Larger batches retain a preallocated result vector, including insertion before
/// node destruction. The registry returns outcomes in ascending task-ID order.
pub(crate) enum TerminalTasks {
    /// Common empty or single-completion result without a result allocation.
    Inline(Option<TerminalTask>),
    /// Multiple outcomes whose storage is reserved before removing task nodes.
    Batch(Vec<TerminalTask>),
}

/// Returns one control pass's terminal work and optional diagnostic transition.
pub(crate) struct ControlResult {
    /// Harvested tasks for removing timers and advancing deferred work groups.
    pub(crate) terminal: TerminalTasks,
    /// At least one new causal or named cancellation was applied in this pass.
    #[cfg(feature = "diagnostics")]
    pub(crate) cancellation_applied: bool,
}

impl TerminalTasks {
    /// Appends an outcome, allocating only if a single inline result becomes a pair.
    pub(crate) fn push(&mut self, task: TerminalTask) {
        match self {
            Self::Inline(slot) => {
                if let Some(first) = slot.take() {
                    *self = Self::Batch(vec![first, task]);
                } else {
                    *slot = Some(task);
                }
            }
            Self::Batch(tasks) => tasks.push(task),
        }
    }
}

impl std::ops::Deref for TerminalTasks {
    type Target = [TerminalTask];

    fn deref(&self) -> &Self::Target {
        match self {
            Self::Inline(task) => task.as_slice(),
            Self::Batch(tasks) => tasks,
        }
    }
}

impl IntoIterator for TerminalTasks {
    type Item = TerminalTask;
    type IntoIter =
        std::iter::Chain<std::option::IntoIter<TerminalTask>, std::vec::IntoIter<TerminalTask>>;

    fn into_iter(self) -> Self::IntoIter {
        let (single, batch) = match self {
            Self::Inline(task) => (task, Vec::new()),
            Self::Batch(tasks) => (None, tasks),
        };
        single.into_iter().chain(batch)
    }
}

/// Reserves task identity and work before the actor constructs its execution carrier.
///
/// Preparation and registration are one synchronous actor phase. Abandoning a
/// reservation releases its leases, but does not undo generation membership;
/// callers must not resume ordinary control processing after an aborted phase.
pub(crate) struct PreparedTask {
    /// Reserved identity used to construct provenance before node insertion.
    id: u64,
    /// Exact structural incarnation inherited by the eventual task node.
    scope: ScopeToken,
    /// Causal owner whose work is retained during carrier construction.
    lineage: Arc<LineageState>,
    /// Signal used by weak sender authority before runtime registration.
    completion: Arc<TaskCompletion>,
    /// Generation liveness captured during this uninterrupted actor phase.
    cancelled: bool,
    /// Immutable memberships already reserved in matching cancellation records.
    generations: Box<[GenerationToken]>,
    /// Causal lease transferred to the task node when registration succeeds.
    work: LineageWork,
    /// Named polling ancestry transferred to the runtime wrapper or actor timer.
    ancestry: OperationAncestry,
    /// Named leases transferred separately so destruction precedes completion.
    operations: OperationWork,
}

impl PreparedTask {
    /// Returns the reserved ID for timer and deferred-group bookkeeping.
    pub(crate) fn id(&self) -> TaskId {
        self.id
    }

    /// Creates weak send authority without retaining the task's causal work.
    pub(crate) fn sender_token(&self) -> std::sync::Weak<TaskCompletion> {
        Arc::downgrade(&self.completion)
    }

    /// Copies generation authority into independently owned sender provenance.
    pub(crate) fn generation_tokens(&self) -> Box<[GenerationToken]> {
        self.generations.clone()
    }

    /// Shares polling ancestry without acquiring another named-work lease.
    pub(crate) fn operation_ancestry(&self) -> OperationAncestry {
        self.ancestry.clone()
    }

    /// Transfers named leases to the runtime guard or actor-owned timer payload.
    pub(crate) fn take_operation_work(&mut self) -> OperationWork {
        std::mem::take(&mut self.operations)
    }
}

/// Owns actor-local task membership and cancellation-generation bookkeeping.
///
/// Shared atomics let runtime workers report completion without borrowing this
/// registry. Its methods run on the actor and do not hold locks across user code.
pub(crate) struct TaskRegistry {
    /// Registered tasks remain present until the actor harvests their outcome.
    nodes: HashMap<u64, TaskNode>,
    /// User cancellation keys map to concurrent ordinary or named generations.
    cancellations: HashMap<CancellationIdentity, CancellationEntry>,
    /// False rules out named records; true may survive until the next pass.
    may_have_operations: bool,
    /// Next never-reused task ID; overflow is a terminal exhaustion error.
    next_id: u64,
    /// Next never-reused generation ID, independent of cancellation key reuse.
    next_generation: u64,
    /// Next opaque cancellation identity used only by diagnostic snapshots.
    #[cfg(feature = "test-util")]
    next_cancellation_diagnostic_id: u64,
    /// Runtime and named-work release notifications wake this actor.
    control: Arc<ControlWake>,
}

impl TaskRegistry {
    /// Starts empty and allocates membership storage only when work arrives.
    pub(crate) fn new(control: Arc<ControlWake>) -> Self {
        Self {
            nodes: HashMap::new(),
            cancellations: HashMap::new(),
            may_have_operations: false,
            next_id: 0,
            next_generation: 0,
            #[cfg(feature = "test-util")]
            next_cancellation_diagnostic_id: 0,
            control,
        }
    }

    /// Starts an ordinary generation with one compiler holder.
    pub(crate) fn register_generation(
        &mut self,
        identity: CancellationIdentity,
    ) -> GenerationToken {
        self.register_generation_with_completion(identity, None)
    }

    /// Starts a named generation whose lifetime includes all captured causal work.
    pub(crate) fn register_named_generation(
        &mut self,
        identity: CancellationIdentity,
        completion: Arc<LineageState>,
    ) -> GenerationToken {
        self.register_generation_with_completion(identity, Some(completion))
    }

    /// Registers one generation, retaining named completion only when supplied.
    ///
    /// # Panics
    ///
    /// Panics if the registry exhausts its monotone generation or diagnostic IDs.
    fn register_generation_with_completion(
        &mut self,
        identity: CancellationIdentity,
        completion: Option<Arc<LineageState>>,
    ) -> GenerationToken {
        self.may_have_operations |= completion.is_some();
        let generation = self.next_generation;
        self.next_generation = self
            .next_generation
            .checked_add(1)
            .expect("cancellation generation space exhausted");
        let live = Arc::new(AtomicBool::new(true));
        let entry = match self.cancellations.entry(identity.clone()) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => {
                #[cfg(feature = "test-util")]
                let diagnostic_id = {
                    let diagnostic_id = self.next_cancellation_diagnostic_id;
                    self.next_cancellation_diagnostic_id = self
                        .next_cancellation_diagnostic_id
                        .checked_add(1)
                        .expect("cancellation diagnostic ID space exhausted");
                    diagnostic_id
                };
                entry.insert(CancellationEntry {
                    #[cfg(feature = "test-util")]
                    diagnostic_id,
                    active: HashMap::new(),
                })
            }
        };
        entry.active.insert(
            generation,
            GenerationRecord {
                live: Arc::clone(&live),
                tasks: HashSet::new(),
                holders: 1,
                completion: completion.clone(),
            },
        );
        GenerationToken {
            identity,
            generation,
            live,
            completion,
        }
    }

    /// Revokes all current generations and returns newly cancelled actor task IDs.
    ///
    /// Runtime tasks must still exit before normal harvesting. Named records stay
    /// reachable until their work finishes; ordinary generations retire immediately.
    /// Returned actor IDs have no ordering guarantee.
    pub(crate) fn cancel_identity(&mut self, identity: &CancellationIdentity) -> Vec<TaskId> {
        let Some(mut entry) = self.cancellations.remove(identity) else {
            return Vec::new();
        };
        let mut actor_tasks = Vec::new();
        for record in entry.active.values() {
            record.live.store(false, Ordering::Release);
            if let Some(completion) = &record.completion {
                completion.cancel();
            }
            for task in &record.tasks {
                if let Some(node) = self.nodes.get(task) {
                    match &node.authority {
                        TaskAuthority::Runtime(abort) => abort.abort(),
                        TaskAuthority::Actor => {
                            if node.completion.finish(TaskStatus::Cancelled) {
                                actor_tasks.push(*task);
                            }
                        }
                    }
                }
            }
        }
        entry.active.retain(|_, record| {
            record
                .completion
                .as_ref()
                .is_some_and(|completion| !completion.is_finished())
        });
        if !entry.active.is_empty() {
            self.cancellations.insert(identity.clone(), entry);
        }
        actor_tasks
    }

    /// Counts unfinished generations for an identity produced by a named declaration.
    pub(crate) fn operation_status(&mut self, identity: &CancellationIdentity) -> OperationStatus {
        self.prune_operations();
        let active = self
            .cancellations
            .get(identity)
            .map_or(0, |entry| entry.active.len());
        OperationStatus::new(active)
    }

    /// Captures current named completions without retaining their work or later generations.
    pub(crate) fn capture_operation(&mut self, identity: &CancellationIdentity) -> OperationWait {
        self.prune_operations();
        let captured = self
            .cancellations
            .get(identity)
            .map_or_else(Vec::new, |entry| {
                entry
                    .active
                    .values()
                    .filter_map(|record| record.completion.clone())
                    .collect()
            });
        OperationWait::new(captured)
    }

    /// Removes finished named records without applying pending cancellation requests.
    pub(crate) fn prune_operations(&mut self) {
        self.maintain_operations(
            false,
            #[cfg(feature = "diagnostics")]
            &mut false,
        );
    }

    /// Fuses named retirement and optional cancellation into one record traversal.
    ///
    /// The no-named-work flag skips the traversal entirely. Call only after all
    /// reservations in the current actor phase have been registered.
    fn maintain_operations(
        &mut self,
        cancel: bool,
        #[cfg(feature = "diagnostics")] cancellation_applied: &mut bool,
    ) {
        if !self.may_have_operations {
            return;
        }
        let mut retained_named = false;
        let nodes = &self.nodes;
        self.cancellations.retain(|_, entry| {
            entry.active.retain(|_, record| {
                #[cfg(test)]
                OPERATION_RECORD_VISITS.with(|visits| visits.set(visits.get() + 1));
                let Some(completion) = &record.completion else {
                    return true;
                };
                if completion.is_finished() {
                    return false;
                }
                retained_named = true;
                if cancel && completion.is_cancelled() && record.live.swap(false, Ordering::AcqRel)
                {
                    // A retained causal command can keep this operation alive
                    // without any task nodes; the live transition is the fact.
                    #[cfg(feature = "diagnostics")]
                    {
                        *cancellation_applied = true;
                    };
                    for id in &record.tasks {
                        // Control runs after task registration; harvesting detaches
                        // task IDs before control can run again.
                        let node = nodes.get(id).expect("control processes registered tasks");
                        match &node.authority {
                            TaskAuthority::Runtime(abort) => abort.abort(),
                            TaskAuthority::Actor => {
                                node.completion.finish(TaskStatus::Cancelled);
                            }
                        }
                    }
                }
                true
            });
            !entry.active.is_empty()
        });
        self.may_have_operations = retained_named;
    }

    /// Adds a compiler-frame hold only if this exact generation remains registered.
    pub(crate) fn retain_generation(&mut self, token: &GenerationToken) {
        if let Some(record) = self
            .cancellations
            .get_mut(&token.identity)
            .and_then(|entry| entry.active.get_mut(&token.generation))
            .filter(|record| Arc::ptr_eq(&record.live, &token.live))
        {
            record.holders += 1;
        }
    }

    /// Releases a matching compiler hold and retires work-free ordinary generations.
    ///
    /// Revoked or foreign tokens are ignored. Matching holds must be balanced.
    pub(crate) fn release_generation(&mut self, token: &GenerationToken) {
        self.update_generation(token, |record| {
            debug_assert!(record.holders > 0, "generation hold must be balanced");
            record.holders -= 1;
            record.holders == 0 && record.tasks.is_empty()
        });
    }

    #[cfg(test)]
    pub(crate) fn prepare(
        &mut self,
        scope: ScopeToken,
        lineage: Arc<LineageState>,
        generations: &[GenerationToken],
    ) -> PreparedTask {
        self.prepare_with_operations(scope, lineage, generations, &OperationAncestry::default())
    }

    /// Reserves identity, memberships and leases before constructing task provenance.
    ///
    /// Registration must finish in the same actor phase, before processing control.
    ///
    /// # Panics
    ///
    /// Panics if the registry exhausts its monotone task IDs.
    pub(crate) fn prepare_with_operations(
        &mut self,
        scope: ScopeToken,
        lineage: Arc<LineageState>,
        generations: &[GenerationToken],
        ancestry: &OperationAncestry,
    ) -> PreparedTask {
        let id = self.next_id;
        self.next_id = self
            .next_id
            .checked_add(1)
            .expect("task ID space exhausted");
        let completion = TaskCompletion::new(Arc::clone(&self.control));
        let work = lineage.add_work();
        for token in generations {
            if let Some(record) = self
                .cancellations
                .get_mut(&token.identity)
                .and_then(|entry| entry.active.get_mut(&token.generation))
                .filter(|record| Arc::ptr_eq(&record.live, &token.live))
            {
                record.tasks.insert(id);
            }
        }
        let cancelled = generations.iter().any(|token| !token.is_live());
        PreparedTask {
            id,
            scope,
            lineage,
            completion,
            cancelled,
            generations: generations.into(),
            work,
            ancestry: ancestry.clone(),
            operations: ancestry.retain_work(),
        }
    }

    /// Registers a shared future, omitting named metadata when ancestry is empty.
    pub(crate) fn register(
        &mut self,
        mut prepared: PreparedTask,
        future: impl Future<Output = ()> + Send + 'static,
    ) -> RuntimeFuture {
        let ancestry = prepared.operation_ancestry();
        if ancestry.is_empty() {
            let (registration, guard) = self.register_runtime(prepared, ());
            Box::pin(track_completion(
                ManagedPoll::new(future, ()),
                registration,
                guard,
            ))
        } else {
            let operations = prepared.take_operation_work();
            let (registration, guard) = self.register_runtime(prepared, operations);
            Box::pin(track_completion(
                ManagedPoll::new(future, ancestry),
                registration,
                guard,
            ))
        }
    }

    /// Constructs shared fallible routing after installing its node and unwind guard.
    ///
    /// The extra handle lets routing observe structural aborts within the final
    /// user poll, before the wrapper records completion. Ordinary shared effects
    /// use [`Self::register`] and do not acquire this additional handle.
    pub(crate) fn register_with_cancellation<Fut: Future<Output = ()> + Send + 'static>(
        &mut self,
        mut prepared: PreparedTask,
        make_future: impl FnOnce(AbortHandle) -> Fut,
    ) -> RuntimeFuture {
        let ancestry = prepared.operation_ancestry();
        if ancestry.is_empty() {
            let (registration, guard) = self.register_runtime(prepared, ());
            let future = make_future(registration.handle());
            Box::pin(track_completion(
                ManagedPoll::new(future, ()),
                registration,
                guard,
            ))
        } else {
            let operations = prepared.take_operation_work();
            let (registration, guard) = self.register_runtime(prepared, operations);
            let future = make_future(registration.handle());
            Box::pin(track_completion(
                ManagedPoll::new(future, ancestry),
                registration,
                guard,
            ))
        }
    }

    /// Constructs an owner-local future after installing its node and unwind guard.
    ///
    /// The borrowed registration lets fallible routing obtain an owned abort
    /// handle. Ordinary work ignores it and pays no extra reference-count pair.
    /// The factory runs now, not on first poll; its returned future cannot retain
    /// the registration borrow because it must be `'static`.
    #[cfg(feature = "local")]
    pub(crate) fn register_local<Fut: Future<Output = ()> + 'static>(
        &mut self,
        mut prepared: PreparedTask,
        make_future: impl FnOnce(&AbortRegistration) -> Fut,
    ) -> std::pin::Pin<Box<dyn Future<Output = ()>>> {
        let ancestry = prepared.operation_ancestry();
        if ancestry.is_empty() {
            let (registration, guard) = self.register_runtime(prepared, ());
            let future = make_future(&registration);
            Box::pin(track_completion(
                ManagedPoll::new(future, ()),
                registration,
                guard,
            ))
        } else {
            let operations = prepared.take_operation_work();
            let (registration, guard) = self.register_runtime(prepared, operations);
            let future = make_future(&registration);
            Box::pin(track_completion(
                ManagedPoll::new(future, ancestry),
                registration,
                guard,
            ))
        }
    }

    /// Installs runtime authority and transfers prepared ownership into its node.
    ///
    /// Bookkeeping is shared across concrete future types. The returned guard
    /// covers factory unwinding before a runtime wrapper can take ownership.
    fn register_runtime<W>(
        &mut self,
        prepared: PreparedTask,
        operations: W,
    ) -> (AbortRegistration, CompletionGuard<W>) {
        let PreparedTask {
            id,
            scope,
            lineage,
            completion,
            cancelled,
            generations,
            work,
            ..
        } = prepared;
        // Actor-owned timers never need a runtime abort pair. Preserve the
        // generation decision made during preparation for runtime tasks.
        let (abort, registration) = AbortHandle::new_pair();
        if cancelled {
            abort.abort();
        }
        let guard = CompletionGuard::new(Arc::clone(&completion), operations);
        self.nodes.insert(
            id,
            TaskNode {
                scope,
                lineage,
                completion,
                authority: TaskAuthority::Runtime(abort),
                generations,
                _work: work,
            },
        );
        (registration, guard)
    }

    /// Registers actor-owned work without allocating a runtime abort pair.
    ///
    /// The caller first transfers named leases to its timer payload, then checks
    /// provenance liveness before installing that payload or completing cancellation.
    pub(crate) fn register_actor(&mut self, prepared: PreparedTask) -> TaskId {
        let PreparedTask {
            id,
            scope,
            lineage,
            completion,
            generations,
            work,
            ..
        } = prepared;
        self.nodes.insert(
            id,
            TaskNode {
                scope,
                lineage,
                completion,
                authority: TaskAuthority::Actor,
                generations,
                _work: work,
            },
        );
        id
    }

    /// Publishes the terminal state of a registered actor-owned task once.
    ///
    /// # Panics
    ///
    /// Panics if the task is absent. Debug builds also reject running status or
    /// runtime-owned authority: those indicate an invalid actor phase transition.
    pub(crate) fn complete_actor(&self, task: TaskId, status: TaskStatus) {
        debug_assert!(status != TaskStatus::Running, "terminal status required");
        let node = self.nodes.get(&task).expect("actor task must exist");
        debug_assert!(
            matches!(node.authority, TaskAuthority::Actor),
            "only actor tasks complete synchronously"
        );
        node.completion.finish(status);
    }

    /// Checks both registry membership and the still-running completion signal.
    pub(crate) fn task_is_running(&self, task: TaskId) -> bool {
        self.nodes
            .get(&task)
            .is_some_and(|node| node.completion.is_running())
    }

    /// Applies causal cancellation and harvests acknowledged terminal tasks.
    ///
    /// # Panics
    ///
    /// A runtime panic aborts actor processing rather than continuing with
    /// potentially inconsistent user state.
    pub(crate) fn process_control(&mut self) -> ControlResult {
        #[cfg(feature = "diagnostics")]
        let mut cancellation_applied = false;
        self.maintain_operations(
            true,
            #[cfg(feature = "diagnostics")]
            &mut cancellation_applied,
        );
        for node in self.nodes.values() {
            if node.lineage.is_cancelled() {
                match &node.authority {
                    TaskAuthority::Runtime(abort) => {
                        #[cfg(feature = "diagnostics")]
                        {
                            cancellation_applied |= !abort.is_aborted();
                        };
                        abort.abort();
                    }
                    TaskAuthority::Actor => {
                        let applied = node.completion.finish(TaskStatus::Cancelled);
                        #[cfg(feature = "diagnostics")]
                        {
                            cancellation_applied |= applied;
                        }
                        #[cfg(not(feature = "diagnostics"))]
                        let _ = applied;
                    }
                }
            }
        }

        ControlResult {
            terminal: self.harvest_terminals(),
            #[cfg(feature = "diagnostics")]
            cancellation_applied,
        }
    }

    /// Removes terminal nodes in ID order, keeping the empty and singleton paths inline.
    fn harvest_terminals(&mut self) -> TerminalTasks {
        let mut terminal = self.nodes.iter().filter_map(|(id, node)| {
            (node.completion.status() != TaskStatus::Running).then_some(*id)
        });
        let Some(first) = terminal.next() else {
            return TerminalTasks::Inline(None);
        };
        let Some(second) = terminal.next() else {
            // The iterator has scanned the whole map; keep its only result inline.
            let mut completed = TerminalTasks::Inline(None);
            self.harvest_terminal(first, &mut completed);
            return completed;
        };
        let Some(third) = terminal.next() else {
            // Keep a pair on the stack; only the returned records need a buffer.
            let mut ids = [first, second];
            ids.sort_unstable();
            let mut completed = TerminalTasks::Batch(Vec::with_capacity(2));
            for id in ids {
                self.harvest_terminal(id, &mut completed);
            }
            return completed;
        };
        let mut terminal: Vec<_> = [first, second, third].into_iter().chain(terminal).collect();
        terminal.sort_unstable();
        let mut completed = TerminalTasks::Batch(Vec::with_capacity(terminal.len()));
        for id in terminal {
            self.harvest_terminal(id, &mut completed);
        }
        completed
    }

    /// Resolves final cancellation and records the outcome before releasing causal work.
    ///
    /// The ID must come from this actor's terminal scan. A panicked runtime cancels
    /// its lineage and propagates a fatal actor panic instead of returning an outcome.
    fn harvest_terminal(&mut self, id: TaskId, completed: &mut TerminalTasks) {
        let node = self.nodes.remove(&id).expect("terminal task must exist");
        // Abort can race with an inner future returning Ready. Cancellation
        // wins until the actor harvests completion, but still waits for exit.
        let status = match (&node.authority, node.completion.status()) {
            (TaskAuthority::Runtime(abort), TaskStatus::Completed) if abort.is_aborted() => {
                TaskStatus::Cancelled
            }
            (_, status) => status,
        };
        if matches!(status, TaskStatus::Panicked | TaskStatus::RuntimeDropped) {
            node.lineage.cancel();
        }
        assert_ne!(status, TaskStatus::Panicked, "effect task panicked");
        for token in node.generations.as_ref() {
            self.detach_task(token, id);
        }
        completed.push(TerminalTask { id, status });
    }

    /// Cancels tasks in departed incarnations and returns newly cancelled actor IDs in order.
    pub(crate) fn cancel_scopes(&mut self, transitions: &ScopeTransitionBatch) -> Vec<TaskId> {
        if transitions.is_empty() {
            return Vec::new();
        }

        let mut actor_tasks = Vec::new();
        for (id, node) in &self.nodes {
            if !transitions.contains(node.scope) {
                continue;
            }
            match &node.authority {
                TaskAuthority::Runtime(abort) => abort.abort(),
                TaskAuthority::Actor => {
                    if node.completion.finish(TaskStatus::Cancelled) {
                        actor_tasks.push(*id);
                    }
                }
            }
        }
        actor_tasks.sort_unstable();
        actor_tasks
    }

    /// Revokes all authority during actor teardown without waiting for runtime exit.
    ///
    /// Runtime guards and actor payloads retain their own work until destruction.
    /// This terminal signal alone is not an acknowledgement of payload destruction.
    pub(crate) fn invalidate_all(&mut self) {
        for node in self.nodes.values() {
            node.lineage.cancel();
            node.completion.finish(TaskStatus::Cancelled);
            if let TaskAuthority::Runtime(abort) = &node.authority {
                abort.abort();
            }
        }
        for entry in self.cancellations.values() {
            for record in entry.active.values() {
                record.live.store(false, Ordering::Release);
            }
        }
    }

    /// Lists still-running tasks deterministically without exposing node storage.
    #[cfg(feature = "test-util")]
    pub(crate) fn live_task_ids(&self) -> Vec<TaskId> {
        let mut ids: Vec<_> = self
            .nodes
            .iter()
            .filter_map(|(id, node)| node.completion.is_running().then_some(*id))
            .collect();
        ids.sort_unstable();
        ids
    }

    /// Counts identities, including named records retained until their work finishes.
    #[cfg(feature = "test-util")]
    pub(crate) fn cancellation_count(&self) -> usize {
        self.cancellations.len()
    }

    /// Returns opaque identity metadata without invoking user key formatting.
    #[cfg(feature = "test-util")]
    pub(crate) fn cancellation_identities(
        &self,
    ) -> Vec<crate::testing::CancellationIdentitySnapshot> {
        let mut identities: Vec<_> = self
            .cancellations
            .iter()
            .map(
                |(identity, entry)| crate::testing::CancellationIdentitySnapshot {
                    diagnostic_id: entry.diagnostic_id,
                    scope_id: identity.scope_id(),
                    type_name: identity.type_name(),
                },
            )
            .collect();
        identities.sort_unstable_by_key(|identity| identity.diagnostic_id);
        identities
    }

    /// Removes a harvested membership and retires an unheld ordinary generation.
    fn detach_task(&mut self, token: &GenerationToken, task: u64) {
        self.update_generation(token, |record| {
            record.tasks.remove(&task);
            record.tasks.is_empty() && record.holders == 0
        });
    }

    /// Mutates one exact generation and removes its entry only when lifetime rules permit.
    ///
    /// Keeping the outer entry borrowed avoids a second user-key equality check
    /// during generation retirement. Named completion overrides ordinary holder counts.
    fn update_generation(
        &mut self,
        token: &GenerationToken,
        update: impl FnOnce(&mut GenerationRecord) -> bool,
    ) {
        let remove_entry = self
            .cancellations
            .get_mut(&token.identity)
            .is_some_and(|entry| {
                let Some(record) = entry
                    .active
                    .get_mut(&token.generation)
                    .filter(|record| Arc::ptr_eq(&record.live, &token.live))
                else {
                    return false;
                };
                let removable = update(record);
                if !record
                    .completion
                    .as_ref()
                    .map_or(removable, |completion| completion.is_finished())
                {
                    return false;
                }
                // Keep the matched outer entry borrowed through generation retirement.
                record.live.store(false, Ordering::Release);
                entry.active.remove(&token.generation);
                entry.active.is_empty()
            });
        if remove_entry {
            self.cancellations.remove(&token.identity);
        }
    }
}

impl fmt::Debug for TaskRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TaskRegistry")
            .field("tasks", &self.nodes.len())
            .field("cancellation_identities", &self.cancellations.len())
            .finish_non_exhaustive()
    }
}

impl Drop for TaskRegistry {
    fn drop(&mut self) {
        self.invalidate_all();
    }
}

#[cfg(test)]
ordinary_test_items! {
mod tests {
    use std::marker::PhantomPinned;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Barrier};
    use std::task::{Context, Poll, Waker};

    use super::{TaskRegistry, TaskStatus};
    use crate::effect::CancellationIdentity;
    use crate::effect::lineage::{ControlWake, LineageState};
    use crate::store::scope::{ScopeIdentity, ScopeRegistry, ScopeSlot, ScopeTransitionBatch};

    #[test]
    fn unnamed_ready_registration_does_not_store_named_operation_metadata() {
        for route in [
            0,
            1,
            #[cfg(feature = "local")]
            2,
        ] {
            let control = ControlWake::new();
            let mut registry = TaskRegistry::new(Arc::clone(&control));
            let lineage = LineageState::new(Arc::downgrade(&control));
            let prepared = registry.prepare(ScopeRegistry::for_test().1.root(), Arc::clone(&lineage), &[]);
            let completion = prepared.sender_token().upgrade().unwrap();
            let mut future: Pin<Box<dyn Future<Output = ()>>> = match route {
                0 => registry.register(prepared, std::future::ready(())),
                #[cfg(feature = "local")]
                2 => registry.register_local(prepared, |_| std::future::ready(())),
                _ => registry.register_with_cancellation(prepared, |_| std::future::ready(())),
            };
            // The four-word budget covers abort/completion ownership and
            // terminal state. Empty named-operation metadata must not add storage.
            let bytes = std::mem::size_of_val(future.as_ref().get_ref());
            assert!(bytes <= 4 * std::mem::size_of::<usize>(), "route {route} stores {bytes} bytes");
            lineage.finish_registration();
            assert_eq!(future.as_mut().poll(&mut Context::from_waker(Waker::noop())), Poll::Ready(()));
            assert_eq!(completion.status(), TaskStatus::Completed);
            assert_eq!(registry.process_control().terminal.len(), 1);
            assert!(lineage.is_finished());
        }
    }

    #[test]
    fn unnamed_registration_masks_named_outer_and_restores_self_wait() {
        use crate::effect::operation::{ManagedPoll, OperationAncestry, OperationWait, SelfWait};

        for route in [
            0,
            1,
            #[cfg(feature = "local")]
            2,
        ] {
            let control = ControlWake::new();
            let mut registry = TaskRegistry::new(Arc::clone(&control));
            let lineage = LineageState::new(Arc::downgrade(&control));
            let operation = LineageState::new(Arc::downgrade(&control));
            let prepared = registry.prepare(ScopeRegistry::for_test().1.root(), Arc::clone(&lineage), &[]);
            let inner_operation = Arc::clone(&operation);
            let future = std::future::poll_fn(move |cx| {
                let mut wait = OperationWait::new(vec![Arc::clone(&inner_operation)]);
                assert!(Pin::new(&mut wait).poll(cx).is_pending());
                Poll::Ready(())
            });
            let local_polls = std::rc::Rc::new(std::cell::Cell::new(0));
            let mut inner: Pin<Box<dyn Future<Output = ()>>> = match route {
                0 => registry.register(prepared, future),
                #[cfg(feature = "local")]
                2 => {
                    let local_polls = std::rc::Rc::clone(&local_polls);
                    registry.register_local(prepared, |_| async move {
                        local_polls.set(local_polls.get() + 1);
                        future.await;
                    })
                }
                _ => registry.register_with_cancellation(prepared, |_| future),
            };
            let ancestry = OperationAncestry::default().with_generation(Arc::clone(&operation));
            let outer_operation = Arc::clone(&operation);
            let mut outer = Box::pin(ManagedPoll::new(std::future::poll_fn(move |cx| {
                assert_eq!(inner.as_mut().poll(cx), Poll::Ready(()));
                let mut wait = OperationWait::new(vec![Arc::clone(&outer_operation)]);
                assert_eq!(Pin::new(&mut wait).poll(cx), Poll::Ready(Err(SelfWait)));
                Poll::Ready(())
            }), ancestry));
            lineage.finish_registration();
            assert_eq!(outer.as_mut().poll(&mut Context::from_waker(Waker::noop())), Poll::Ready(()));
            assert_eq!(local_polls.get(), usize::from(route == 2));
            let mut external = OperationWait::new(vec![Arc::clone(&operation)]);
            assert!(Pin::new(&mut external).poll(&mut Context::from_waker(Waker::noop())).is_pending());
            operation.finish_registration();
            assert_eq!(registry.process_control().terminal.len(), 1);
            assert!(lineage.is_finished());
        }
    }

    #[test]
    #[cfg_attr(
        not(feature = "local"),
        expect(
            clippy::single_element_loop,
            reason = "The feature-gated scenario table retains only shared registration without Local"
        )
    )]
    fn unnamed_registration_factory_unwind_releases_payload_before_completion() {
        struct Payload {
            completion: Arc<super::TaskCompletion>,
            observed_running: Arc<AtomicBool>,
            drops: Arc<AtomicUsize>,
        }

        impl Drop for Payload {
            fn drop(&mut self) {
                self.observed_running.store(self.completion.is_running(), Ordering::Relaxed);
                self.drops.fetch_add(1, Ordering::Relaxed);
            }
        }

        for local in [
            false,
            #[cfg(feature = "local")]
            true,
        ] {
            let control = ControlWake::new();
            let mut registry = TaskRegistry::new(Arc::clone(&control));
            let lineage = LineageState::new(Arc::downgrade(&control));
            let prepared = registry.prepare(ScopeRegistry::for_test().1.root(), Arc::clone(&lineage), &[]);
            let completion = prepared.sender_token().upgrade().unwrap();
            let drops = Arc::new(AtomicUsize::new(0));
            let observed_running = Arc::new(AtomicBool::new(false));
            let payload = Payload {
                completion: Arc::clone(&completion),
                observed_running: Arc::clone(&observed_running),
                drops: Arc::clone(&drops),
            };
            lineage.finish_registration();
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let factory = move || -> std::future::Ready<()> {
                    let _payload = payload;
                    panic!("intentional unnamed factory panic");
                };
                match local {
                    #[cfg(feature = "local")]
                    true => drop(registry.register_local(prepared, |_| factory())),
                    _ => drop(registry.register_with_cancellation(prepared, |_| factory())),
                }
            }));
            assert!(result.is_err());
            assert!(observed_running.load(Ordering::Relaxed));
            assert_eq!(drops.load(Ordering::Relaxed), 1);
            assert_eq!(completion.status(), TaskStatus::Panicked);
            assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                registry.process_control();
            })).is_err());
            assert!(lineage.is_cancelled());
            assert!(lineage.is_finished());
        }
    }

    #[test]
    fn unnamed_control_does_not_visit_cancellation_records_for_named_maintenance() {
        let mut registry = TaskRegistry::new(ControlWake::new());
        for key in 0..16 {
            registry.register_generation(CancellationIdentity::local(key));
        }
        super::OPERATION_RECORD_VISITS.with(|visits| visits.set(0));
        assert!(registry.process_control().terminal.is_empty());
        let visits = super::OPERATION_RECORD_VISITS.with(std::cell::Cell::get);
        eprintln!("ordinary cancellation records=16, named-maintenance record visits={visits}, registry_bytes={}, task_node_bytes={}, prepared_task_bytes={}",
            std::mem::size_of::<TaskRegistry>(),
            std::mem::size_of::<super::TaskNode>(),
            std::mem::size_of::<super::PreparedTask>());
        eprintln!("layout: command={} action_command={} provenance={} operation_work={} identity={} authority={} task_request={}",
            std::mem::size_of::<crate::store::command::Command<(), ()>>(),
            std::mem::size_of::<crate::store::command::ActionCommand<()>>(),
            std::mem::size_of::<crate::store::command::Provenance>(),
            std::mem::size_of::<crate::effect::operation::OperationWork>(),
            std::mem::size_of::<CancellationIdentity>(),
            std::mem::size_of::<crate::store::scope::ScopeAuthority>(),
            std::mem::size_of::<crate::store::task_context::TaskRequest<(), ()>>());
        assert_eq!(visits, 0, "never-named work must not scan cancellation records for named maintenance");
        assert_eq!(registry.cancellations.len(), 16);
    }

    #[test]
    fn named_maintenance_fuses_visits_and_returns_to_zero_after_real_retirement() {
        let control = ControlWake::new();
        let mut registry = TaskRegistry::new(Arc::clone(&control));
        let ordinary: Vec<_> = (0..16).map(|key| {
            registry.register_generation(CancellationIdentity::local(key))
        }).collect();
        let identity = crate::effect::operation::OperationDeclaration::identity("work");
        let first = LineageState::new(Arc::downgrade(&control));
        let second = LineageState::new(Arc::downgrade(&control));
        let one = registry.register_named_generation(identity.clone(), Arc::clone(&first));
        assert_eq!(registry.cancel_identity(&identity), [] as [u64; 0]);
        let two = registry.register_named_generation(identity.clone(), Arc::clone(&second));
        assert!(first.is_cancelled());
        assert!(!first.is_finished(), "replacement keeps actual registration ownership");
        super::OPERATION_RECORD_VISITS.with(|visits| visits.set(0));
        assert!(registry.process_control().terminal.is_empty());
        assert_eq!(super::OPERATION_RECORD_VISITS.with(std::cell::Cell::get), 18,
            "one fused pass visits each of sixteen ordinary and two named records once");
        assert!(registry.may_have_operations);
        registry.invalidate_all();
        registry.prune_operations();
        assert!(registry.may_have_operations, "invalidation does not destroy retained payload ownership");
        first.finish_registration();
        second.finish_registration();
        registry.release_generation(&one);
        registry.release_generation(&two);
        registry.prune_operations();
        assert!(!registry.may_have_operations);
        assert_eq!(registry.cancellations.len(), 16);
        super::OPERATION_RECORD_VISITS.with(|visits| visits.set(0));
        assert!(registry.process_control().terminal.is_empty());
        assert_eq!(super::OPERATION_RECORD_VISITS.with(std::cell::Cell::get), 0);

        let later = LineageState::new(Arc::downgrade(&control));
        let later_token = registry.register_named_generation(identity, Arc::clone(&later));
        assert!(registry.may_have_operations);
        later.finish_registration();
        registry.release_generation(&later_token);
        registry.prune_operations();
        assert!(!registry.may_have_operations, "later registration can retire back to the empty fast path");
        for token in ordinary { registry.release_generation(&token); }
        assert!(registry.cancellations.is_empty(), "ordinary retirement remains unchanged");
    }

    struct NamedDropProbe {
        operation: Arc<LineageState>,
        observed_pending: Arc<AtomicBool>,
        drops: Arc<AtomicUsize>,
        panic: bool,
    }

    impl Future for NamedDropProbe {
        type Output = ();

        fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<()> {
            Poll::Ready(())
        }
    }

    impl Drop for NamedDropProbe {
        fn drop(&mut self) {
            self.observed_pending.store(!self.operation.is_finished(), Ordering::Relaxed);
            self.drops.fetch_add(1, Ordering::Relaxed);
            assert!(!self.panic, "intentional named payload destructor panic");
        }
    }

    #[test]
    fn named_concrete_destructor_panic_releases_after_payload_and_wakes_control() {
        use crate::effect::operation::{OperationAncestry, OperationDeclaration, OperationRegistration};

        for (local, poll_ready) in [
            (false, false),
            (false, true),
            #[cfg(feature = "local")]
            (true, false),
            #[cfg(feature = "local")]
            (true, true),
        ] {
            let control = ControlWake::new();
            let mut registry = TaskRegistry::new(Arc::clone(&control));
            let operation = LineageState::new(Arc::downgrade(&control));
            let registration = OperationRegistration::new(Arc::clone(&operation));
            let identity = OperationDeclaration::identity("panicking body");
            let generation = registry.register_named_generation(identity.clone(), Arc::clone(&operation));
            let ancestry = OperationAncestry::default().with_generation(Arc::clone(&operation));
            let lineage = LineageState::new(Arc::downgrade(&control));
            let prepared = registry.prepare_with_operations(ScopeRegistry::for_test().1.root(), Arc::clone(&lineage), &[generation], &ancestry);
            let completion = prepared.sender_token().upgrade().unwrap();
            let observed_pending = Arc::new(AtomicBool::new(false));
            let drops = Arc::new(AtomicUsize::new(0));
            let payload = NamedDropProbe {
                operation: Arc::clone(&operation),
                observed_pending: Arc::clone(&observed_pending),
                drops: Arc::clone(&drops),
                panic: true,
            };
            let mut wrapper: Pin<Box<dyn Future<Output = ()>>> = match local {
                #[cfg(feature = "local")]
                true => {
                    let payload = std::rc::Rc::new(payload);
                    registry.register_local(prepared, |_| std::future::poll_fn(move |_| {
                        // Retain the !Send payload after Ready until the concrete future drops.
                        assert_eq!(std::rc::Rc::strong_count(&payload), 1);
                        Poll::Ready(())
                    }))
                }
                _ => registry.register(prepared, payload),
            };
            drop(registration);
            lineage.finish_registration();
            let mut wait = Box::pin(registry.capture_operation(&identity));
            assert!(wait.as_mut().poll(&mut Context::from_waker(Waker::noop())).is_pending());
            let before = control.epoch();
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
                if poll_ready {
                    let _ = wrapper.as_mut().poll(&mut Context::from_waker(Waker::noop()));
                } else {
                    drop(wrapper);
                }
            }));
            assert!(result.is_err());
            assert!(observed_pending.load(Ordering::Relaxed));
            assert_eq!(drops.load(Ordering::Relaxed), 1);
            assert_eq!(completion.status(), TaskStatus::Panicked);
            assert_eq!(wait.as_mut().poll(&mut Context::from_waker(Waker::noop())), Poll::Ready(Ok(())));
            assert!(control.epoch() > before);
            registry.prune_operations();
            assert!(registry.cancellations.is_empty());
        }
    }

    #[test]
    #[cfg_attr(
        not(feature = "local"),
        expect(
            clippy::single_element_loop,
            reason = "The feature-gated scenario table retains only shared registration without Local"
        )
    )]
    fn named_registration_factory_unwind_releases_payload_before_completion() {
        use crate::effect::operation::{OperationAncestry, OperationDeclaration, OperationRegistration};

        for local in [
            false,
            #[cfg(feature = "local")]
            true,
        ] {
            let control = ControlWake::new();
            let mut registry = TaskRegistry::new(Arc::clone(&control));
            let operation = LineageState::new(Arc::downgrade(&control));
            let registration = OperationRegistration::new(Arc::clone(&operation));
            let identity = OperationDeclaration::identity("panicking factory");
            let generation = registry.register_named_generation(identity.clone(), Arc::clone(&operation));
            let ancestry = OperationAncestry::default().with_generation(Arc::clone(&operation));
            let prepared = registry.prepare_with_operations(ScopeRegistry::for_test().1.root(), LineageState::new(Arc::downgrade(&control)), &[generation], &ancestry);
            let completion = prepared.sender_token().upgrade().unwrap();
            let observed_pending = Arc::new(AtomicBool::new(false));
            let drops = Arc::new(AtomicUsize::new(0));
            let payload = NamedDropProbe {
                operation: Arc::clone(&operation),
                observed_pending: Arc::clone(&observed_pending),
                drops: Arc::clone(&drops),
                panic: false,
            };
            drop(registration);
            let mut wait = Box::pin(registry.capture_operation(&identity));
            assert!(wait.as_mut().poll(&mut Context::from_waker(Waker::noop())).is_pending());
            let before = control.epoch();
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let factory = move || -> std::future::Ready<()> {
                    let _payload = payload;
                    panic!("intentional named factory panic");
                };
                match local {
                    #[cfg(feature = "local")]
                    true => drop(registry.register_local(prepared, |_| factory())),
                    _ => drop(registry.register_with_cancellation(prepared, |_| factory())),
                }
            }));
            assert!(result.is_err());
            assert!(observed_pending.load(Ordering::Relaxed));
            assert_eq!(drops.load(Ordering::Relaxed), 1);
            assert_eq!(completion.status(), TaskStatus::Panicked);
            assert_eq!(wait.as_mut().poll(&mut Context::from_waker(Waker::noop())), Poll::Ready(Ok(())));
            assert!(control.epoch() > before);
            registry.prune_operations();
            assert!(registry.cancellations.is_empty());
        }
    }

    #[test]
    fn cancelling_terminal_actor_work_preserves_completion_and_live_sibling() {
        // A completed actor task can still be registered until control harvesting.
        for by_scope in [false, true] {
            let control = ControlWake::new();
            let mut registry = TaskRegistry::new(Arc::clone(&control));
            let (_scopes_lifecycle, mut scopes) = ScopeRegistry::for_test();
            let root = scopes.root();
            let node = ScopeRegistry::node(&mut ScopeSlot::new());
            let scope = scopes.mount(root, node, ScopeIdentity::unit());
            let lineage = LineageState::new(Arc::downgrade(&control));
            let identity = CancellationIdentity::local("retiring work").in_scope(scope);
            let generation = registry.register_generation(identity.clone());
            let ids: Vec<_> = (0..2)
                .map(|_| {
                    let prepared = registry.prepare(
                        scope,
                        Arc::clone(&lineage),
                        std::slice::from_ref(&generation),
                    );
                    registry.register_actor(prepared)
                })
                .collect();
            let sibling = registry.prepare(root, Arc::clone(&lineage), &[]);
            let sibling = registry.register_actor(sibling);
            lineage.finish_registration();
            registry.complete_actor(ids[0], TaskStatus::Completed);
            let mut transitions = ScopeTransitionBatch::default();
            transitions.extend(scopes.leave(scope));

            let cancelled = if by_scope {
                registry.cancel_scopes(&transitions)
            } else {
                registry.cancel_identity(&identity)
            };
            assert_eq!(cancelled, [ids[1]]);
            let epoch = control.epoch();
            let repeated = if by_scope {
                registry.cancel_scopes(&transitions)
            } else {
                registry.cancel_identity(&identity)
            };
            assert_eq!(repeated, [] as [u64; 0]);
            assert_eq!(control.epoch(), epoch);
            assert!(registry.task_is_running(sibling));
            assert!(!lineage.is_finished());
            let terminal = registry.process_control().terminal;
            let outcomes: Vec<_> = terminal.iter().map(|task| (task.id, task.status)).collect();
            assert_eq!(
                outcomes,
                [(ids[0], TaskStatus::Completed), (ids[1], TaskStatus::Cancelled)]
            );
            assert!(registry.process_control().terminal.is_empty());
            assert!(registry.task_is_running(sibling));
            registry.release_generation(&generation);
            assert!(!generation.is_live());

            registry.complete_actor(sibling, TaskStatus::Completed);
            let terminal = registry.process_control().terminal;
            assert_eq!(terminal.len(), 1);
            assert_eq!(
                (terminal[0].id, terminal[0].status),
                (sibling, TaskStatus::Completed)
            );
            assert!(lineage.is_finished());
            assert!(!lineage.is_cancelled());
        }
    }

    // Repeated outer lookups invoke user equality even for one cancellation identity.
    #[test]
    fn generation_retirement_reuses_the_matched_identity_entry() {
        struct CountedIdentity(Arc<AtomicUsize>);

        impl PartialEq for CountedIdentity {
            fn eq(&self, _: &Self) -> bool {
                self.0.fetch_add(1, Ordering::SeqCst);
                true
            }
        }
        impl Eq for CountedIdentity {}
        impl std::hash::Hash for CountedIdentity {
            fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
                state.write_u8(0);
            }
        }

        let mut comparison_counts = Vec::new();
        for (task_finishes_last, keep_sibling) in [
            (false, false),
            (false, true),
            (true, false),
            (true, true),
        ] {
            let comparisons = Arc::new(AtomicUsize::new(0));
            let control = ControlWake::new();
            let mut registry = TaskRegistry::new(Arc::clone(&control));
            let lineage = LineageState::new(Arc::downgrade(&control));
            let scope = ScopeRegistry::for_test().1.root();
            let identity = CancellationIdentity::local(CountedIdentity(Arc::clone(&comparisons)))
                .in_scope(scope);
            let generation = registry.register_generation(identity.clone());
            let sibling = keep_sibling
                .then(|| registry.register_generation(identity.clone()));
            let prepared = registry.prepare(
                scope,
                Arc::clone(&lineage),
                std::slice::from_ref(&generation),
            );
            let id = registry.register_actor(prepared);
            lineage.finish_registration();

            // A valid foreign token can have the same identity and generation number.
            let mut foreign = TaskRegistry::new(Arc::clone(&control));
            let foreign_generation = foreign.register_generation(identity);
            assert_eq!(foreign_generation.generation, generation.generation);
            registry.release_generation(&foreign_generation);
            registry.detach_task(&foreign_generation, id);
            let entry = registry.cancellations.values().next().unwrap();
            let record = &entry.active[&generation.generation];
            assert_eq!(record.holders, 1);
            assert!(record.tasks.contains(&id));
            assert!(foreign_generation.is_live());
            assert!(generation.is_live());

            if task_finishes_last {
                registry.release_generation(&generation);
                assert!(generation.is_live());
                assert!(!lineage.is_finished());
                comparisons.store(0, Ordering::SeqCst);
                registry.complete_actor(id, TaskStatus::Completed);
                let terminal = registry.process_control().terminal;
                assert_eq!(terminal.len(), 1);
                assert_eq!(terminal[0].id, id);
                assert_eq!(terminal[0].status, TaskStatus::Completed);
            } else {
                registry.complete_actor(id, TaskStatus::Completed);
                let terminal = registry.process_control().terminal;
                assert_eq!(terminal.len(), 1);
                assert_eq!(terminal[0].id, id);
                assert_eq!(terminal[0].status, TaskStatus::Completed);
                assert!(generation.is_live());
                comparisons.store(0, Ordering::SeqCst);
                registry.release_generation(&generation);
            }
            comparison_counts.push(comparisons.load(Ordering::SeqCst));
            assert!(!generation.is_live());
            assert_eq!(Arc::strong_count(&generation.live), 1);
            assert!(lineage.is_finished());
            assert!(registry.nodes.is_empty());
            assert_eq!(registry.cancellations.len(), usize::from(keep_sibling));
            if let Some(sibling) = sibling {
                assert!(sibling.is_live());
                let entry = registry.cancellations.values().next().unwrap();
                assert_eq!(entry.active.len(), 1);
                assert!(entry.active.contains_key(&sibling.generation));
                registry.release_generation(&generation);
                assert!(sibling.is_live(), "stale release cannot retire the sibling");
                registry.release_generation(&sibling);
                assert!(!sibling.is_live());
                assert!(registry.cancellations.is_empty());
            }
            assert!(foreign_generation.is_live());
            foreign.release_generation(&foreign_generation);
            assert!(!foreign_generation.is_live());
        }
        assert_eq!(comparison_counts, [2, 1, 2, 1]);
    }

    struct ReadyDrop {
        // Record ordering instead of asserting during a possibly unwinding Drop.
        dropped: Arc<AtomicBool>,
        drops: Arc<AtomicUsize>,
        completion: Arc<super::TaskCompletion>,
        panic_on_drop: bool,
        _pinned: PhantomPinned,
    }

    impl Future for ReadyDrop {
        type Output = ();

        fn poll(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<()> {
            Poll::Ready(())
        }
    }

    impl Drop for ReadyDrop {
        fn drop(&mut self) {
            self.dropped
                .store(self.completion.is_running(), Ordering::Relaxed);
            let previous = self.drops.fetch_add(1, Ordering::Relaxed);
            assert!(
                !(self.panic_on_drop && previous == 0),
                "intentional input drop panic"
            );
        }
    }

    fn assert_unpolled_input_drop_status(panic_on_drop: bool, expected: TaskStatus) {
        let control = ControlWake::new();
        let mut registry = TaskRegistry::new(Arc::clone(&control));
        let lineage = LineageState::new(Arc::downgrade(&control));
        let scope = ScopeRegistry::for_test().1.root();
        let prepared = registry.prepare(scope, lineage, &[]);
        let completion = prepared
            .sender_token()
            .upgrade()
            .expect("prepared completion");
        let dropped = Arc::new(AtomicBool::new(false));
        let drops = Arc::new(AtomicUsize::new(0));
        let wrapper = registry.register(
            prepared,
            ReadyDrop {
                dropped: Arc::clone(&dropped),
                drops: Arc::clone(&drops),
                completion: Arc::clone(&completion),
                panic_on_drop,
                _pinned: PhantomPinned,
            },
        );
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(wrapper)));
        assert_eq!(result.is_err(), panic_on_drop);
        assert!(dropped.load(Ordering::Relaxed));
        assert_eq!(drops.load(Ordering::Relaxed), 1);
        assert_eq!(completion.status(), expected);
    }

    #[test]
    fn unpolled_concrete_input_drops_before_runtime_drop_publication() {
        assert_unpolled_input_drop_status(false, TaskStatus::RuntimeDropped);
    }

    #[test]
    fn unpolled_concrete_input_drop_panic_is_published_as_panicked() {
        assert_unpolled_input_drop_status(true, TaskStatus::Panicked);
    }

    #[test]
    fn ready_input_drop_panic_is_published_without_a_second_drop() {
        let control = ControlWake::new();
        let mut registry = TaskRegistry::new(Arc::clone(&control));
        let lineage = LineageState::new(Arc::downgrade(&control));
        let scope = ScopeRegistry::for_test().1.root();
        let prepared = registry.prepare(scope, lineage, &[]);
        let completion = prepared
            .sender_token()
            .upgrade()
            .expect("prepared completion");
        let dropped = Arc::new(AtomicBool::new(false));
        let drops = Arc::new(AtomicUsize::new(0));
        let mut wrapper = registry.register(
            prepared,
            ReadyDrop {
                dropped: Arc::clone(&dropped),
                drops: Arc::clone(&drops),
                completion: Arc::clone(&completion),
                panic_on_drop: true,
                _pinned: PhantomPinned,
            },
        );
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            wrapper
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
        }))
        .expect_err("input destructor panic must escape polling");
        assert!(dropped.load(Ordering::Relaxed));
        assert_eq!(completion.status(), TaskStatus::Panicked);
        assert_eq!(drops.load(Ordering::Relaxed), 1);
        drop(wrapper);
        assert_eq!(drops.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn concrete_pinned_input_drops_before_ready_wrapper_is_released() {
        let control = ControlWake::new();
        let mut registry = TaskRegistry::new(Arc::clone(&control));
        let lineage = LineageState::new(Arc::downgrade(&control));
        let scope = ScopeRegistry::for_test().1.root();
        let prepared = registry.prepare(scope, Arc::clone(&lineage), &[]);
        let completion = prepared
            .sender_token()
            .upgrade()
            .expect("prepared completion");
        let dropped = Arc::new(AtomicBool::new(false));
        let drops = Arc::new(AtomicUsize::new(0));
        let mut wrapper = registry.register(
            prepared,
            ReadyDrop {
                dropped: Arc::clone(&dropped),
                drops: Arc::clone(&drops),
                completion: Arc::clone(&completion),
                panic_on_drop: false,
                _pinned: PhantomPinned,
            },
        );
        lineage.finish_registration();
        assert_eq!(
            format!("{registry:?}"),
            "TaskRegistry { tasks: 1, cancellation_identities: 0, .. }"
        );
        assert_eq!(drops.load(Ordering::Relaxed), 0);
        assert_eq!(completion.status(), TaskStatus::Running);
        assert!(!dropped.load(Ordering::Relaxed));
        assert!(
            wrapper
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_ready()
        );
        assert!(dropped.load(Ordering::Relaxed));
        assert_eq!(completion.status(), TaskStatus::Completed);
        let terminal = registry.process_control().terminal;
        assert_eq!(terminal.len(), 1);
        assert_eq!(terminal[0].status, TaskStatus::Completed);
        assert!(lineage.is_finished());
        assert_eq!(
            format!("{registry:?}"),
            "TaskRegistry { tasks: 0, cancellation_identities: 0, .. }"
        );
        // The completed outer allocation deliberately remains alive through harvest.
        drop(wrapper);
        assert_eq!(drops.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn cancelling_an_unlaunched_reservation_releases_ownership_on_abandonment() {
        let control = ControlWake::new();
        let mut registry = TaskRegistry::new(Arc::clone(&control));
        let lineage = LineageState::new(Arc::downgrade(&control));
        let scope = ScopeRegistry::for_test().1.root();
        let identity = CancellationIdentity::local("unlaunched").in_scope(scope);
        let generation = registry.register_generation(identity.clone());
        let prepared = registry.prepare(
            scope,
            Arc::clone(&lineage),
            std::slice::from_ref(&generation),
        );
        let completion = prepared.sender_token();
        assert!(registry.nodes.is_empty());
        assert!(
            registry.cancellations[&identity].active[&generation.generation]
                .tasks
                .contains(&prepared.id())
        );
        lineage.finish_registration();
        let mut finish = Box::pin(lineage.finish());
        let mut context = Context::from_waker(Waker::noop());
        assert!(finish.as_mut().poll(&mut context).is_pending());

        assert_eq!(registry.cancel_identity(&identity), [] as [u64; 0]);
        assert!(!generation.is_live());
        assert!(registry.cancellations.is_empty());
        assert!(registry.process_control().terminal.is_empty());
        assert_eq!(
            completion
                .upgrade()
                .expect("reservation owns completion")
                .status(),
            TaskStatus::Running
        );
        assert!(finish.as_mut().poll(&mut context).is_pending());

        drop(prepared);
        assert!(completion.upgrade().is_none());
        assert!(finish.as_mut().poll(&mut context).is_ready());
        assert_eq!(Arc::strong_count(&generation.live), 1);
        assert!(registry.nodes.is_empty());
        assert!(registry.process_control().terminal.is_empty());
    }

    #[test]
    fn cancelled_generation_never_polls_prepared_runtime_input() {
        let control = ControlWake::new();
        let mut registry = TaskRegistry::new(Arc::clone(&control));
        let lineage = LineageState::new(Arc::downgrade(&control));
        let scope = ScopeRegistry::for_test().1.root();
        let identity = CancellationIdentity::local("cancelled").in_scope(scope);
        let generation = registry.register_generation(identity.clone());
        registry.cancel_identity(&identity);
        let prepared = registry.prepare(scope, Arc::clone(&lineage), &[generation]);
        let polled = Arc::new(AtomicBool::new(false));
        let input_polled = Arc::clone(&polled);
        let wrapper = registry.register(
            prepared,
            std::future::poll_fn(move |_| {
                input_polled.store(true, Ordering::Relaxed);
                Poll::Ready(())
            }),
        );
        lineage.finish_registration();
        futures_executor::block_on(wrapper);
        assert!(!polled.load(Ordering::Relaxed));
        let terminal = registry.process_control().terminal;
        assert_eq!(terminal.len(), 1);
        assert_eq!(terminal[0].status, TaskStatus::Cancelled);
        assert!(lineage.is_finished());
    }

    #[test]
    fn singleton_and_batched_harvest_preserve_id_order_and_running_tasks() {
        let control = ControlWake::new();
        let mut registry = TaskRegistry::new(Arc::clone(&control));
        let lineage = LineageState::new(Arc::downgrade(&control));
        let scope = ScopeRegistry::for_test().1.root();
        let ids: Vec<_> = (0..8)
            .map(|_| {
                let prepared = registry.prepare(scope, Arc::clone(&lineage), &[]);
                let id = prepared.id();
                registry.register_actor(prepared);
                id
            })
            .collect();
        lineage.finish_registration();
        assert!(registry.process_control().terminal.is_empty());

        registry.complete_actor(ids[3], TaskStatus::Completed);
        let singleton = registry.process_control().terminal;
        assert_eq!(singleton.len(), 1);
        assert_eq!(singleton[0].id, ids[3]);
        assert_eq!(singleton[0].status, TaskStatus::Completed);
        assert_eq!(registry.nodes.len(), 7);
        assert!(!lineage.is_finished());

        registry.complete_actor(ids[6], TaskStatus::Completed);
        registry.complete_actor(ids[0], TaskStatus::Cancelled);
        registry.complete_actor(ids[4], TaskStatus::Completed);
        let batch = registry.process_control().terminal;
        assert_eq!(
            batch
                .iter()
                .map(|task| (task.id, task.status))
                .collect::<Vec<_>>(),
            [
                (ids[0], TaskStatus::Cancelled),
                (ids[4], TaskStatus::Completed),
                (ids[6], TaskStatus::Completed),
            ]
        );
        assert_eq!(registry.nodes.len(), 4);
        for index in [1, 2, 5, 7] {
            assert!(registry.task_is_running(ids[index]));
        }
        assert!(registry.process_control().terminal.is_empty());
        assert!(!lineage.is_finished());

        registry.complete_actor(ids[7], TaskStatus::Cancelled);
        registry.complete_actor(ids[1], TaskStatus::Completed);
        let pair = registry.process_control().terminal;
        assert_eq!(pair.len(), 2);
        assert_eq!(
            (pair[0].id, pair[0].status),
            (ids[1], TaskStatus::Completed)
        );
        assert_eq!(
            (pair[1].id, pair[1].status),
            (ids[7], TaskStatus::Cancelled)
        );
        assert_eq!(registry.nodes.len(), 2);
        assert!(registry.task_is_running(ids[2]));
        assert!(registry.task_is_running(ids[5]));
        assert!(!lineage.is_finished());
    }

    #[test]
    fn cancellation_during_ready_poll_remains_cancelled_after_runtime_exit() {
        let control = ControlWake::new();
        let mut registry = TaskRegistry::new(Arc::clone(&control));
        let lineage = LineageState::new(Arc::downgrade(&control));
        let scope = ScopeRegistry::for_test().1.root();
        let identity = CancellationIdentity::local("sequence").in_scope(scope);
        let generation = registry.register_generation(identity.clone());
        let prepared = registry.prepare(scope, Arc::clone(&lineage), &[generation]);
        lineage.finish_registration();
        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let task_entered = Arc::clone(&entered);
        let task_release = Arc::clone(&release);
        let wrapper = registry.register(
            prepared,
            Box::pin(std::future::poll_fn(move |_| {
                task_entered.wait();
                task_release.wait();
                std::task::Poll::Ready(())
            })),
        );
        let worker = std::thread::spawn(move || futures_executor::block_on(wrapper));

        entered.wait();
        registry.cancel_identity(&identity);
        let before_runtime_exit = registry.process_control().terminal;
        let finished_before_runtime_exit = lineage.is_finished();
        release.wait();
        worker.join().expect("runtime worker exits normally");
        let terminal = registry.process_control().terminal;

        assert!(
            before_runtime_exit.is_empty(),
            "cancellation must wait for runtime exit"
        );
        assert_eq!(terminal.len(), 1);
        assert_eq!(terminal[0].status, TaskStatus::Cancelled);
        assert!(!finished_before_runtime_exit);
        assert!(lineage.is_finished());
    }

    #[test]
    fn cancellation_after_runtime_exit_before_harvest_wins() {
        let control = ControlWake::new();
        let mut registry = TaskRegistry::new(Arc::clone(&control));
        let lineage = LineageState::new(Arc::downgrade(&control));
        let scope = ScopeRegistry::for_test().1.root();
        let identity = CancellationIdentity::local("sequence").in_scope(scope);
        let generation = registry.register_generation(identity.clone());
        let prepared = registry.prepare(scope, lineage, &[generation]);
        let wrapper = registry.register(prepared, Box::pin(std::future::ready(())));

        futures_executor::block_on(wrapper);
        registry.cancel_identity(&identity);
        let terminal = registry.process_control().terminal;

        assert_eq!(terminal.len(), 1);
        assert_eq!(terminal[0].status, TaskStatus::Cancelled);
    }

    #[test]
    fn dropping_unpolled_runtime_wrapper_aborts_the_runtime() {
        let control = ControlWake::new();
        let mut registry = TaskRegistry::new(Arc::clone(&control));
        let lineage = LineageState::new(Arc::downgrade(&control));
        let scope = crate::store::scope::ScopeRegistry::for_test().1.root();
        let prepared = registry.prepare(scope, Arc::clone(&lineage), &[]);
        let completion = prepared.sender_token();
        let wrapper = registry.register(prepared, Box::pin(std::future::pending()));

        drop(wrapper);

        assert_eq!(
            completion
                .upgrade()
                .expect("registry owns completion")
                .status(),
            TaskStatus::RuntimeDropped
        );
        let terminal = registry.process_control().terminal;
        assert_eq!(terminal.len(), 1);
        assert_eq!(terminal[0].status, TaskStatus::RuntimeDropped);
        assert!(lineage.is_cancelled());
    }

    #[test]
    fn unwinding_runtime_wrapper_is_a_runtime_panic() {
        let control = ControlWake::new();
        let mut registry = TaskRegistry::new(Arc::clone(&control));
        let lineage = LineageState::new(Arc::downgrade(&control));
        let scope = crate::store::scope::ScopeRegistry::for_test().1.root();
        let prepared = registry.prepare(scope, Arc::clone(&lineage), &[]);
        let completion = prepared.sender_token();
        let wrapper = registry.register(prepared, Box::pin(std::future::pending()));

        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _wrapper = wrapper;
            panic!("intentional runtime unwind");
        }));

        assert!(panic.is_err());
        assert_eq!(
            completion
                .upgrade()
                .expect("registry owns completion")
                .status(),
            TaskStatus::Panicked
        );
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            registry.process_control();
        }));
        assert!(panic.is_err());
        assert!(lineage.is_cancelled());
    }

    #[test]
    fn cancelling_scope_aborts_descendant_tasks_only() {
        let control = ControlWake::new();
        let mut registry = TaskRegistry::new(Arc::clone(&control));
        let (_scopes_lifecycle, mut scopes) = ScopeRegistry::for_test();
        let root = scopes.root();
        let parent_node = ScopeRegistry::node(&mut ScopeSlot::new());
        let child_node = ScopeRegistry::node(&mut ScopeSlot::new());
        let sibling_node = ScopeRegistry::node(&mut ScopeSlot::new());
        let parent = scopes.mount(root, parent_node, ScopeIdentity::unit());
        let child = scopes.mount(parent, child_node, ScopeIdentity::unit());
        let sibling = scopes.mount(root, sibling_node, ScopeIdentity::unit());

        let parent_lineage = LineageState::new(Arc::downgrade(&control));
        let parent_prepared = registry.prepare(parent, parent_lineage, &[]);
        let parent_completion = parent_prepared.sender_token();
        let parent_wrapper = registry.register(parent_prepared, Box::pin(std::future::pending()));

        let child_lineage = LineageState::new(Arc::downgrade(&control));
        let child_prepared = registry.prepare(child, child_lineage, &[]);
        let child_id = child_prepared.id();
        let child_completion = child_prepared.sender_token();
        registry.register_actor(child_prepared);

        let sibling_lineage = LineageState::new(Arc::downgrade(&control));
        let sibling_prepared = registry.prepare(sibling, sibling_lineage, &[]);
        let sibling_completion = sibling_prepared.sender_token();
        let sibling_wrapper = registry.register(sibling_prepared, Box::pin(std::future::pending()));

        let mut transitions = ScopeTransitionBatch::default();
        transitions.extend(scopes.leave(parent));
        let cancelled_actor_tasks = registry.cancel_scopes(&transitions);

        assert_eq!(cancelled_actor_tasks, [child_id]);
        assert_eq!(
            child_completion
                .upgrade()
                .expect("registry retains child completion")
                .status(),
            TaskStatus::Cancelled
        );
        futures_executor::block_on(parent_wrapper);
        assert_eq!(
            parent_completion
                .upgrade()
                .expect("registry retains parent completion")
                .status(),
            TaskStatus::Cancelled
        );
        assert_eq!(
            sibling_completion
                .upgrade()
                .expect("registry retains sibling completion")
                .status(),
            TaskStatus::Running
        );
        drop(sibling_wrapper);
    }

    #[cfg(debug_assertions)]
    #[test]
    fn runtime_task_rejects_actor_completion_without_losing_ownership() {
        let control = ControlWake::new();
        let mut registry = TaskRegistry::new(Arc::clone(&control));
        let lineage = LineageState::new(Arc::downgrade(&control));
        let scope = ScopeRegistry::for_test().1.root();
        let identity = CancellationIdentity::local("runtime authority").in_scope(scope);
        let generation = registry.register_generation(identity.clone());
        let prepared = registry.prepare(
            scope,
            Arc::clone(&lineage),
            std::slice::from_ref(&generation),
        );
        let id = prepared.id();
        let completion = prepared.sender_token();
        let capture = Arc::new(());
        let captured = Arc::downgrade(&capture);
        let wrapper = registry.register(prepared, async move { drop(capture) });
        lineage.finish_registration();
        let epoch = control.epoch();

        // Private phase misuse: runtime authority must not be completed by the actor.
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            registry.complete_actor(id, TaskStatus::Completed);
        }))
        .expect_err("runtime-owned work must reject actor completion");

        assert_eq!(control.epoch(), epoch);
        assert!(registry.task_is_running(id));
        assert_eq!(registry.nodes.len(), 1);
        assert!(matches!(
            registry.nodes[&id].authority,
            super::TaskAuthority::Runtime(_)
        ));
        assert!(!lineage.is_finished());
        assert!(!lineage.is_cancelled());
        assert_eq!(captured.strong_count(), 1);
        let record = &registry.cancellations[&identity].active[&generation.generation];
        assert_eq!(record.holders, 1);
        assert_eq!(record.tasks.len(), 1);
        assert!(record.tasks.contains(&id));
        assert!(generation.is_live());

        futures_executor::block_on(wrapper);
        assert!(captured.upgrade().is_none());
        let terminal = registry.process_control().terminal;
        assert_eq!(terminal.len(), 1);
        assert_eq!((terminal[0].id, terminal[0].status), (id, TaskStatus::Completed));
        assert!(registry.nodes.is_empty());
        assert!(lineage.is_finished());
        assert!(completion.upgrade().is_none());
        registry.release_generation(&generation);
        assert!(!generation.is_live());
        assert!(registry.cancellations.is_empty());
    }
}
}

#[cfg(all(test, feature = "allocation-measurement"))]
pub(crate) use allocation_tests::{
    actor_task_registration_allocates_only_completion, paired_harvest_allocates_only_result_buffer,
};

#[cfg(all(test, feature = "allocation-measurement"))]
mod allocation_tests {
    use std::sync::Arc;

    use super::{TaskRegistry, TaskStatus};
    use crate::effect::lineage::{ControlWake, LineageState};
    use crate::store::scope::ScopeRegistry;

    #[cfg(feature = "allocation-measurement")]
    pub(crate) fn actor_task_registration_allocates_only_completion() {
        if crate::allocation_test::run_in_child(
            "effect::registry::tests::actor_task_registration_allocates_only_completion",
        ) {
            return;
        }
        let control = ControlWake::new();
        let mut registry = TaskRegistry::new(Arc::clone(&control));
        let lineage = LineageState::new(Arc::downgrade(&control));
        let scope = ScopeRegistry::for_test().1.root();
        let warm = registry.prepare(scope, Arc::clone(&lineage), &[]);
        let warm_id = registry.register_actor(warm);
        registry.complete_actor(warm_id, TaskStatus::Completed);
        assert_eq!(registry.process_control().terminal.len(), 1);

        // Keep registry capacity warm; actor-owned work needs no runtime abort pair.
        let profiler = dhat::Profiler::builder().testing().build();
        let before = dhat::HeapStats::get();
        let prepared = registry.prepare(scope, Arc::clone(&lineage), &[]);
        let id = registry.register_actor(prepared);
        let after = dhat::HeapStats::get();
        let blocks = after.total_blocks - before.total_blocks;
        let bytes = after.total_bytes - before.total_bytes;
        assert!(registry.task_is_running(id));
        lineage.finish_registration();
        registry.complete_actor(id, TaskStatus::Completed);
        let terminal = registry.process_control().terminal;
        assert_eq!(terminal.len(), 1);
        assert_eq!(terminal[0].id, id);
        assert!(lineage.is_finished());
        drop(profiler);
        assert!(
            blocks <= 1,
            "actor registration allocated {blocks} blocks / {bytes} bytes; budget is 1 block"
        );
    }

    #[cfg(feature = "allocation-measurement")]
    pub(crate) fn paired_harvest_allocates_only_result_buffer() {
        if crate::allocation_test::run_in_child(
            "effect::registry::tests::paired_harvest_allocates_only_result_buffer",
        ) {
            return;
        }
        let control = ControlWake::new();
        let mut registry = TaskRegistry::new(Arc::clone(&control));
        let lineage = LineageState::new(Arc::downgrade(&control));
        let scope = ScopeRegistry::for_test().1.root();
        let ids: Vec<_> = (0..3)
            .map(|_| {
                let prepared = registry.prepare(scope, Arc::clone(&lineage), &[]);
                registry.register_actor(prepared)
            })
            .collect();
        lineage.finish_registration();
        registry.complete_actor(ids[2], TaskStatus::Cancelled);
        registry.complete_actor(ids[0], TaskStatus::Completed);

        let profiler = dhat::Profiler::builder().testing().build();
        let before = dhat::HeapStats::get();
        let terminal = registry.process_control().terminal;
        let after = dhat::HeapStats::get();
        let blocks = after.total_blocks - before.total_blocks;
        let bytes = after.total_bytes - before.total_bytes;
        assert_eq!(terminal.len(), 2);
        assert_eq!(
            (terminal[0].id, terminal[0].status),
            (ids[0], TaskStatus::Completed)
        );
        assert_eq!(
            (terminal[1].id, terminal[1].status),
            (ids[2], TaskStatus::Cancelled)
        );
        assert!(registry.task_is_running(ids[1]));
        assert!(!lineage.is_finished());
        drop(profiler);
        assert_eq!(
            blocks, 1,
            "paired harvest allocated {blocks} blocks / {bytes} bytes"
        );
        assert_eq!(
            bytes,
            (2 * std::mem::size_of::<super::TerminalTask>()) as u64
        );
    }
}
