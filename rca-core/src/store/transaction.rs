//! Commits serial mutations and returns registered, unpolled launches.
//!
//! A commit validates entry, runs the mutation, reconciles child lifetimes,
//! prepares reactions, stamps the revision, registers work, publishes observations
//! and then acknowledges the caller. The actor starts returned work afterwards.
//! This ordering is not rollback: callbacks and destructors can panic or close the
//! Store, and reply guards retain the last phase actually reached.
//!
//! Shared and Local execution use the same borrowed state machine. Carrier-specific
//! callbacks and futures retain their original ownership and thread bounds.

use super::actor::{
    Launches, Sequence, SequenceId, TimerKind, TimerPayload, TimerProvenance, WorkGroup,
};
use super::carriers::{Carriers, EffectLeaf, EffectOf, Execution, Shared};
use super::command::{ActionCommand, Command, CommitError, CommitSender, Provenance};
use super::ingress::CommandIngress;
use super::mount::MountDeclaration;
use super::mounted::{
    DeferredEffect, DeferredProgram, DeferredQueue, DeferredTimer, Delivery, InstalledNode,
    MountedApply, MountedCall, MountedWork, RootDelivery, WorkOrigin,
};
use super::observation::ObservationRegistryCore;
use super::output::OutputCall;
use super::scope::{HandleRegistryCore, ScopeRegistry, ScopeToken, ScopeTransitionBatch};
use super::task_context::{
    CommitReply, EnteredSend, ReadCall, TaskAccessError, TaskAccessReason, TaskProgress,
    TaskRequest, TypedSend, UpdateCall,
};
use super::{ActionTask, Ingress};
use crate::effect::leaves::{EffectLeaves, EffectValue};
use crate::effect::lineage::{ControlWake, LineageState};
use crate::effect::operation::{
    OperationAncestry, OperationDeclaration, OperationPolicy, OperationRegistration,
};
use crate::effect::registry::{GenerationToken, TaskId, TaskRegistry, TaskStatus, TerminalTask};
use crate::effect::timer::{TimerId, TimerQueue};
use crate::effect::{EffectSendFuture, Program};
use crate::feature::{ReduceDriver, Work, WorkItem};
use crate::{DependencyValues, EffectSendError, MonotonicInstant, ReduceContext};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};

/// Transfers registered work and continuation state back to the actor.
pub(super) struct Turn<F = crate::RuntimeFuture> {
    /// Whether the actor should continue after this result is handled.
    pub(super) flow: bool,
    /// Whether successful launch also completes structural installation readiness.
    pub(super) installation: bool,
    /// Registered futures that have not yet been polled or transferred to a runtime.
    pub(super) launches: Launches<F>,
    /// Actual mutation outcome retained for assertion-frontier inspection.
    #[cfg(feature = "test-util")]
    pub(super) receipt: Option<Result<ActionTask, TaskAccessError>>,
}

impl<F> Turn<F> {
    /// Records rejection without launches; dispatch decides whether to continue.
    pub(super) fn rejected(error: TaskAccessError) -> Self {
        #[cfg(not(feature = "test-util"))]
        let _ = error;
        Self {
            flow: false,
            installation: false,
            launches: Launches::default(),
            #[cfg(feature = "test-util")]
            receipt: Some(Err(error)),
        }
    }

    /// Returns a non-mutating actor decision without an assertion receipt.
    pub(super) fn empty(flow: bool) -> Self {
        Self {
            flow,
            installation: false,
            launches: Launches::default(),
            #[cfg(feature = "test-util")]
            receipt: None,
        }
    }
}

/// Borrows the actor resources needed for one synchronous transaction phase.
///
/// No part owns the actor or executor. Callbacks may enqueue later commands or
/// close admission, but cannot re-enter these exclusive state/map borrows.
pub(crate) struct TransactionParts<'a, State, Action, C: Carriers<State, Action> = Shared> {
    /// Root value accessed only within the current serial phase.
    pub(super) state: &'a mut State,
    /// Feature driver that owns reducer and hook state.
    pub(super) reducer: &'a mut C::Driver,
    /// Dependency environment used by mutation and hook contexts.
    pub(super) dependencies: &'a mut DependencyValues,
    /// Issue endpoint captured into contexts without changing its lifetime policy.
    pub(super) issues: &'a crate::IssueReporter,
    /// Optional synchronous diagnostics callback, which may panic or close the Store.
    #[cfg(feature = "diagnostics")]
    pub(super) lifecycle_sink: Option<&'a mut C::LifecycleSink>,
    /// Weak command endpoint captured by effect senders and typed task contexts.
    pub(super) ingress: &'a Weak<CommandIngress<Command<State, Action, C>>>,
    /// Owns task identities, cancellation membership and causal completion leases.
    pub(super) registry: &'a mut TaskRegistry,
    /// Current incarnation authority and the child-lifetime reconciliation state.
    pub(super) scopes: &'a mut ScopeRegistry,
    /// Installed child reducers retained until their scopes retire.
    pub(super) mounted: &'a mut Vec<Box<C::Node>>,
    /// Ordered declarations used to reconcile children and freeze reaction rosters.
    pub(super) mounts: &'a mut Vec<Box<C::Mount>>,
    /// Whether roster traversal needs nested recipe depth ordering.
    pub(super) has_mount_recipes: bool,
    /// State selectors whose handles must refresh after each mutation.
    pub(super) handles: &'a mut HandleRegistryCore<C::HandleSelector>,
    /// Actor wake signal captured weakly by newly created causal lineages.
    pub(super) control: &'a Arc<ControlWake>,
    /// Monotone revision stamped before work registration and observation publication.
    pub(super) commit_version: &'a mut u64,
    /// Reverse task-to-group index consumed when terminal work is harvested.
    pub(super) task_groups: &'a mut HashMap<TaskId, Arc<WorkGroup>>,
    /// Active delays indexed separately from their owned completion barriers.
    pub(super) sequence_waits: &'a mut HashMap<SequenceId, TaskId>,
    /// Deferred suffixes and their retained authority/work leases.
    pub(super) sequences: &'a mut HashMap<SequenceId, Sequence<DeferredQueue<State, Action, C>>>,
    /// Never-reused identity source for deferred sequences.
    pub(super) next_sequence_id: &'a mut SequenceId,
    /// Actor-owned delayed payloads, ordered by deadline and insertion.
    pub(super) timers: &'a mut TimerQueue<TimerPayload<State, Action, C>>,
    /// Reverse task-to-timer index used before terminal task accounting.
    pub(super) task_timers: &'a mut HashMap<TaskId, TimerId>,
    /// Live observers closed on retirement and published after registration.
    pub(super) observations: &'a mut ObservationRegistryCore<State, C::ObservationEntry>,
    /// One clock sample supplied by the actor for this transaction.
    pub(super) now: MonotonicInstant,
    /// Optional assertion boundary that receives the committed resource snapshot.
    #[cfg(feature = "test-util")]
    pub(super) assertion_gate: &'a Option<crate::testing::AssertionGateBinding<Action>>,
    /// Installation failure guard consumed by structural commit, when present.
    #[cfg(feature = "test-util")]
    pub(super) structural_terminal: Option<super::task_context::TerminalGuard>,
}

/// Owns one mutation request until validation rejects it or user code enters it.
pub(super) enum Mutation<'a, S, A, C: Carriers<S, A> = Shared> {
    /// Reconciles installation and initial hooks without an action payload.
    Structural,
    /// Legacy action with optional cancellation of its eventual receipt wait.
    Action(
        ActionCommand<A>,
        Option<&'a crate::observation::wait::WaitCancellation>,
    ),
    /// Typed action whose rejection can recover the original unentered input.
    Send(TypedSend<A>, Provenance, super::scope::ScopeAuthority),
    /// Owned state callback with causal provenance and frozen authority.
    Update(Box<C::Update>, Provenance, super::scope::ScopeAuthority),
    /// Child action whose route and child input remain owned until entry.
    Mounted(Box<C::Mounted>, Provenance, super::scope::ScopeAuthority),
    /// Child output whose handler chain must freeze before user callbacks run.
    Output(Box<C::Output>, Provenance, super::scope::ScopeAuthority),
}

/// Retains returned work before reconciliation permits registration.
pub(crate) enum Batch<S, A, C: Carriers<S, A>> {
    /// Root effects and typed tasks in their declared order.
    Root(Work<EffectOf<S, A, C>, C::Task>),
    /// Child-typed work with the scope needed to register its routed leaves.
    Mounted {
        /// Optional erased child work; an empty return needs no allocation.
        work: Option<Box<C::MountedWork>>,
        /// Incarnation that owns the returned child work.
        scope: ScopeToken,
    },
}

impl<S, A, C: Carriers<S, A>> Batch<S, A, C> {
    /// Checks whether this batch can add any work to the causal lineage.
    pub(super) fn is_empty(&self) -> bool {
        match self {
            Self::Root(work) => work.is_empty(),
            Self::Mounted { work, .. } => work.as_ref().is_none_or(|work| work.is_empty()),
        }
    }

    /// Drops effects whose tagged scope retired before this batch was registered.
    fn prune_scopes(&mut self, current: &dyn Fn(ScopeToken) -> bool) {
        match self {
            Self::Root(work) => work.for_each_effect(|effect| {
                effect.prune_scopes(&|scope| current(scope));
            }),
            Self::Mounted { work, .. } => {
                if let Some(work) = work {
                    work.prune_scopes(current);
                }
            }
        }
    }
}

/// Keeps the entered request's terminal owner through stamping and publication.
enum Reply<A, R> {
    /// Structural installation has no action reply channel.
    Structural,
    /// Legacy acknowledgement with a separate optional assertion result.
    Legacy {
        /// Commit outcome sent only after registered work and publication finish.
        reply: CommitSender,
        /// Assertion result that shares the committed causal task when enabled.
        #[cfg(feature = "test-util")]
        assertion_reply: Option<async_channel::Sender<Result<ActionTask, crate::StoreStopped>>>,
    },
    /// Entered action guard; the original input is no longer recoverable.
    Send(EnteredSend<A>),
    /// Entered callback guard that owns its eventual result.
    Update(R),
}

impl<A, R: CommitReply> Reply<A, R> {
    /// Records committed progress before later registration/publication may panic.
    fn stamp(&mut self, revision: u64) {
        match self {
            Self::Structural => {}
            Self::Legacy { reply, .. } => {
                #[cfg(feature = "test-util")]
                reply.mark_committed(revision);
                #[cfg(not(feature = "test-util"))]
                let _ = reply;
            }
            Self::Send(reply) => reply.mark_committed(revision),
            Self::Update(reply) => reply.mark_committed(revision),
        }
    }

    /// Transfers the final receipt once, after the commit's owned work is registered.
    fn complete(self, task: ActionTask) {
        match self {
            Self::Structural => {}
            Self::Legacy {
                reply,
                #[cfg(feature = "test-util")]
                assertion_reply,
            } => {
                #[cfg(feature = "test-util")]
                if let Some(assertion_reply) = assertion_reply {
                    let _ = assertion_reply.try_send(Ok(task.clone()));
                }
                let _ = reply.send(Ok(task));
            }
            Self::Send(reply) => reply.complete(task),
            Self::Update(reply) => reply.complete(task),
        }
    }
}

/// Runs one mutation through reconciliation, registration, publication and reply.
///
/// Returned futures remain unpolled. Callback panic unwinds the actor, with reply
/// guards retaining entry or stamped-commit progress; state is not rolled back.
#[expect(
    clippy::too_many_lines,
    reason = "one commit sequence keeps reduction, invalidation, publication and acknowledgement ordering visible"
)]
pub(super) fn commit<S: 'static, A: 'static, C: Execution<S, A>>(
    mut parts: TransactionParts<'_, S, A, C>,
    mutation: Mutation<'_, S, A, C>,
) -> Turn<C::Future> {
    let installation = matches!(mutation, Mutation::Structural);
    #[cfg(feature = "test-util")]
    let mut structural_terminal = parts
        .structural_terminal
        .take()
        .unwrap_or_else(|| super::task_context::TerminalGuard::new(None));
    #[cfg(feature = "diagnostics")]
    if installation
        && parts.scopes.installation_is_live()
        && let Some(sink) = parts.lifecycle_sink.as_deref_mut()
    {
        #[cfg(feature = "test-util")]
        structural_terminal.mark_entered();
        crate::diagnostics::LifecycleCall::new(C::lifecycle_sink(sink), *parts.commit_version)
            .emit(crate::diagnostics::LifecycleKind::Installed {
                lifetime: parts.scopes.root().diagnostic_id(),
                parent: None,
                configuration: Some(parts.scopes.configuration(parts.scopes.root())),
            });
    }
    // Retain causal command ownership until all successor work is registered.
    let mut operation_work = crate::effect::operation::OperationWork::default();
    let mut transitions = ScopeTransitionBatch::default();
    let mut output_work = Vec::new();
    let has_reactions = parts.mounts.iter().any(|mount| mount.has_reactions());
    let mut captures = super::reaction::Frame::default();
    let (mut work, provenance, mut reply, cancellation): (Batch<S, A, C>, _, _, _) = match mutation
    {
        Mutation::Structural => (
            Batch::Root(Work::default()),
            Provenance::External { handle: None },
            Reply::Structural,
            None,
        ),
        Mutation::Action(command, cancellation) => {
            let ActionCommand {
                input,
                reply,
                #[cfg(feature = "test-util")]
                assertion_reply,
                operation_work: command_work,
            } = command;
            #[cfg(feature = "test-util")]
            let mut reply = reply;
            operation_work = command_work;
            let (action, provenance) = input.into_parts();
            let mut progress = TaskProgress::NotStarted;
            let rejection = if !provenance.is_live()
                || provenance
                    .scope()
                    .is_some_and(|scope| !parts.scopes.is_current(scope))
            {
                Some((CommitError::Cancelled, TaskAccessReason::Cancelled))
            } else if provenance.handle().is_some_and(|handle| {
                !parts.handles.is_current(parts.state, handle, || {
                    progress = TaskProgress::MayHaveRun;
                    #[cfg(feature = "test-util")]
                    reply.mark_entered();
                })
            }) {
                Some((CommitError::ScopeEnded, TaskAccessReason::ScopeEnded))
            } else {
                None
            };
            if let Some((error, reason)) = rejection {
                let access_error = TaskAccessError::new(reason, progress);
                #[cfg(feature = "test-util")]
                reply.record_terminal(Err(access_error));
                #[cfg(feature = "test-util")]
                if let Some(assertion_reply) = assertion_reply {
                    let _ = assertion_reply.try_send(Err(crate::StoreStopped::new()));
                }
                let _ = reply.send(Err(error));
                drop(action);
                return Turn::rejected(access_error);
            }
            #[cfg(feature = "test-util")]
            reply.mark_entered();
            let action = action.into_action();
            if has_reactions
                && !super::reaction::capture::<S, A, C>(
                    parts.mounts,
                    parts.state,
                    parts.scopes,
                    &mut captures,
                )
            {
                let error =
                    TaskAccessError::new(TaskAccessReason::StoreClosed, TaskProgress::MayHaveRun);
                #[cfg(feature = "test-util")]
                reply.record_terminal(Err(error));
                #[cfg(feature = "test-util")]
                if let Some(assertion_reply) = assertion_reply {
                    let _ = assertion_reply.try_send(Err(crate::StoreStopped::new()));
                }
                drop(reply);
                return Turn::rejected(error);
            }
            let context = ReduceContext::new(
                parts.dependencies,
                parts.now,
                parts.scopes,
                &mut transitions,
            )
            .with_issue_reporter(parts.issues.clone());
            #[cfg(feature = "diagnostics")]
            let context = context.with_lifecycle_diagnostics(
                parts.lifecycle_sink.as_deref_mut().map(|sink| {
                    crate::diagnostics::LifecycleCall::new(
                        C::lifecycle_sink(sink),
                        *parts.commit_version,
                    )
                }),
            );
            let work = parts.reducer.reduce(parts.state, &action, context);
            (
                Batch::Root(work),
                provenance,
                Reply::Legacy {
                    reply,
                    #[cfg(feature = "test-util")]
                    assertion_reply,
                },
                cancellation,
            )
        }
        Mutation::Send(request, provenance, authority) => {
            if let Some(error) = parts.rejection(&provenance, &authority) {
                request.reject(error);
                return Turn::rejected(error);
            }
            let (action, reply) = request.enter();
            if has_reactions
                && !super::reaction::capture::<S, A, C>(
                    parts.mounts,
                    parts.state,
                    parts.scopes,
                    &mut captures,
                )
            {
                let error =
                    TaskAccessError::new(TaskAccessReason::StoreClosed, TaskProgress::MayHaveRun);
                reply.fail(error);
                return Turn::rejected(error);
            }
            let context = ReduceContext::new(
                parts.dependencies,
                parts.now,
                parts.scopes,
                &mut transitions,
            )
            .with_issue_reporter(parts.issues.clone());
            #[cfg(feature = "diagnostics")]
            let context = context.with_lifecycle_diagnostics(
                parts.lifecycle_sink.as_deref_mut().map(|sink| {
                    crate::diagnostics::LifecycleCall::new(
                        C::lifecycle_sink(sink),
                        *parts.commit_version,
                    )
                }),
            );
            let work = parts.reducer.reduce(parts.state, &action, context);
            (Batch::Root(work), provenance, Reply::Send(reply), None)
        }
        Mutation::Output(mut call, provenance, authority) => {
            if let Some(error) = parts.rejection(&provenance, &authority) {
                call.reject(error);
                return Turn::rejected(error);
            }
            if let Err(error) = call.freeze(parts.scopes) {
                call.reject(error);
                return Turn::rejected(error);
            }
            let context = ReduceContext::new(
                parts.dependencies,
                parts.now,
                parts.scopes,
                &mut transitions,
            )
            .with_issue_reporter(parts.issues.clone());
            #[cfg(feature = "diagnostics")]
            let context = context.with_lifecycle_diagnostics(
                parts.lifecycle_sink.as_deref_mut().map(|sink| {
                    crate::diagnostics::LifecycleCall::new(
                        C::lifecycle_sink(sink),
                        *parts.commit_version,
                    )
                }),
            );
            let mut capture = |state: &S, scopes: &ScopeRegistry| {
                super::reaction::capture::<S, A, C>(parts.mounts, state, scopes, &mut captures)
            };
            let before = has_reactions
                .then_some(&mut capture as &mut super::reaction::BeforeMutation<'_, S>);
            let (reply, work) = match call.apply(parts.state, context, before) {
                Ok(applied) => applied,
                Err(error) => return Turn::rejected(error),
            };
            output_work = work;
            (
                Batch::Root(Work::default()),
                provenance,
                Reply::Update(reply),
                None,
            )
        }
        Mutation::Mounted(call, provenance, authority) => {
            if let Some(error) = parts
                .rejection(&provenance, &authority)
                .or_else(|| call.input_rejection(parts.scopes))
            {
                call.reject(error);
                return Turn::rejected(error);
            }
            let context = ReduceContext::new(
                parts.dependencies,
                parts.now,
                parts.scopes,
                &mut transitions,
            )
            .with_issue_reporter(parts.issues.clone());
            #[cfg(feature = "diagnostics")]
            let context = context.with_lifecycle_diagnostics(
                parts.lifecycle_sink.as_deref_mut().map(|sink| {
                    crate::diagnostics::LifecycleCall::new(
                        C::lifecycle_sink(sink),
                        *parts.commit_version,
                    )
                }),
            );
            let mut capture = |state: &S, scopes: &ScopeRegistry| {
                super::reaction::capture::<S, A, C>(parts.mounts, state, scopes, &mut captures)
            };
            let before = has_reactions
                .then_some(&mut capture as &mut super::reaction::BeforeMutation<'_, S>);
            let (reply, work, scope) = match call.apply(parts.state, context, before) {
                MountedApply::Applied { reply, work, scope } => (reply, work, scope),
                MountedApply::Rejected(error) => return Turn::rejected(error),
            };
            (
                Batch::Mounted { work, scope },
                provenance,
                Reply::Update(reply),
                None,
            )
        }
        Mutation::Update(mut request, provenance, authority) => {
            if let Some(error) = parts.rejection(&provenance, &authority) {
                request.reject(error);
                return Turn::rejected(error);
            }
            if has_reactions {
                request.mark_entered();
                if !super::reaction::capture::<S, A, C>(
                    parts.mounts,
                    parts.state,
                    parts.scopes,
                    &mut captures,
                ) {
                    let error = TaskAccessError::new(
                        TaskAccessReason::StoreClosed,
                        TaskProgress::MayHaveRun,
                    );
                    request.reject(error);
                    return Turn::rejected(error);
                }
            }
            let reply = match request.apply(parts.state) {
                super::task_context::Apply::Applied(reply) => reply,
                super::task_context::Apply::Rejected(error) => return Turn::rejected(error),
            };
            (
                Batch::Root(Work::default()),
                provenance,
                Reply::Update(reply),
                None,
            )
        }
    };

    #[cfg(feature = "test-util")]
    structural_terminal.mark_entered();
    super::mount::nested::reconcile_roster::<S, A, C>(
        parts.mounts,
        parts.has_mount_recipes,
        parts.state,
        parts.scopes,
        &mut transitions,
        parts.mounted,
        #[cfg(feature = "diagnostics")]
        parts.lifecycle_sink.as_deref_mut().map(|sink| {
            crate::diagnostics::LifecycleCall::new(C::lifecycle_sink(sink), *parts.commit_version)
        }),
    );
    parts.scopes.invalidate_outputs(&transitions);
    parts.handles.refresh(parts.state);
    let cancelled_actor_tasks = parts.registry.cancel_scopes(&transitions);
    #[cfg(feature = "diagnostics")]
    if let Some(sink) = parts.lifecycle_sink.as_deref_mut() {
        let mut call =
            crate::diagnostics::LifecycleCall::new(C::lifecycle_sink(sink), *parts.commit_version);
        for token in transitions.iter() {
            call.emit(crate::diagnostics::LifecycleKind::CancellationRequested {
                cause: crate::diagnostics::CancellationCause::ScopeRetirement,
                scope: Some(token.diagnostic_id()),
            });
        }
    }
    let cancelled_timers = parts
        .timers
        .cancel_scopes(&transitions, |timer| timer.provenance.scope);
    for timer in cancelled_timers {
        parts.task_timers.remove(&timer.task);
    }
    parts.remove_cancelled_timers(cancelled_actor_tasks);
    if !transitions.is_empty() {
        parts.prune_deferred_scopes();
        work.prune_scopes(&|scope| parts.scopes.is_current(scope));
        for work in &mut output_work {
            work.prune_scopes(&|scope| parts.scopes.is_current(scope));
        }
        parts
            .mounted
            .retain(|node| parts.scopes.is_current(node.scope()));
    }
    parts.observations.close_scopes(&transitions);
    // Freeze metadata before invoking any hook. Factories can construct owned
    // work, but cannot discover another registration or poll it in this turn.
    let mut initial_roster = Vec::new();
    let mut root_initial = None;
    // Only recipe-enabled rosters need the depth stack. The ordinary flat path
    // keeps its existing single finite roster allocation and declaration order.
    let mut parents = Vec::new();
    for (index, mount) in parts.mounts.iter().enumerate() {
        let scope = mount.reaction_entry().map(|(scope, _)| scope);
        if scope == Some(parts.scopes.root()) {
            root_initial = scope.map(|scope| (index, scope));
            continue;
        }
        if parts.has_mount_recipes {
            while let Some((_, entry)) = parents.pop_if(|(depth, _)| *depth >= mount.depth()) {
                initial_roster.push(entry);
            }
        }
        if let Some(scope) = scope {
            let entry = (index, scope);
            if parts.has_mount_recipes {
                parents.push((mount.depth(), entry));
            } else {
                initial_roster.push(entry);
            }
        }
    }
    initial_roster.extend(parents.into_iter().rev().map(|(_, entry)| entry));
    // The root is the actual enclosing scope, not a depth-zero sibling whose
    // physical registration position could run it before a mounted subtree.
    initial_roster.extend(root_initial);
    let mut initial_work = Vec::new();
    for (index, scope) in initial_roster {
        // The frozen roster cannot change through dependency-only factories
        // while this turn owns State/scopes. Root closure can still happen in a
        // callback (or on another thread), and must prevent the next entry.
        if !parts.scopes.installation_is_live() {
            break;
        }
        let context = ReduceContext::new(
            parts.dependencies,
            parts.now,
            parts.scopes,
            &mut transitions,
        )
        .with_issue_reporter(parts.issues.clone())
        .in_scope(scope);
        #[cfg(feature = "diagnostics")]
        let context =
            context.with_lifecycle_diagnostics(parts.lifecycle_sink.as_deref_mut().map(|sink| {
                crate::diagnostics::LifecycleCall::new(
                    C::lifecycle_sink(sink),
                    *parts.commit_version,
                )
            }));
        parts.mounts[index]
            .reaction_entry()
            .expect("frozen reaction entry remains selected")
            .1
            .run(
                parts.state,
                scope,
                context,
                &mut captures,
                &mut initial_work,
            );
    }
    // Unmatched old values can themselves close the root during destruction.
    // Drain them outside guards BEFORE deciding whether returned work is live.
    drop(captures);
    // Discard only not-yet-registered reaction batches, never ordinary work.
    if !initial_work.is_empty() && !parts.scopes.installation_is_live() {
        initial_work.clear();
    }
    let mut lineage = provenance.lineage().cloned();
    let had_lineage = lineage.is_some();
    let mut generations =
        if work.is_empty() && output_work.iter().all(Batch::is_empty) && initial_work.is_empty() {
            Vec::new()
        } else {
            provenance.generations().to_vec()
        };
    let inherited_generations = generations.len();
    let operations = provenance.operations();
    *parts.commit_version = parts
        .commit_version
        .checked_add(1)
        .expect("Store commit version space exhausted");
    reply.stamp(*parts.commit_version);
    #[cfg(feature = "test-util")]
    structural_terminal.mark_committed(*parts.commit_version);
    let mut launches = Launches::default();
    let mut register = |work: Batch<S, A, C>| match work {
        Batch::Mounted { work, scope } => {
            if let Some(work) = work {
                work.register(
                    &mut parts,
                    WorkOrigin {
                        scope,
                        lineage: &mut lineage,
                        generations: &mut generations,
                        operations: &operations,
                        inherited_generations,
                        group: None,
                    },
                    &mut launches,
                );
            }
        }
        Batch::Root(work) => {
            let scope = parts.scopes.root();
            register_root_work(
                work,
                &mut parts,
                WorkOrigin {
                    scope,
                    lineage: &mut lineage,
                    generations: &mut generations,
                    operations: &operations,
                    inherited_generations,
                    group: None,
                },
                &mut launches,
            );
        }
    };
    if output_work.is_empty() && initial_work.is_empty() {
        // Keep empty backing buffers' destruction at the original loop boundary,
        // including before queued launches if primary registration unwinds.
        let empty_secondary = (output_work, initial_work);
        register(work);
        drop(empty_secondary);
    } else {
        for work in std::iter::once(work).chain(output_work).chain(initial_work) {
            register(work);
        }
    }
    let task = lineage
        .as_ref()
        .map_or_else(ActionTask::completed, |lineage| {
            ActionTask::live(Arc::clone(lineage))
        });
    if !had_lineage && let Some(lineage) = &lineage {
        lineage.finish_registration();
    }
    if !parts.observations.is_empty() {
        parts
            .observations
            .publish(parts.state, *parts.commit_version, parts.handles);
    }
    if let Some(cancellation) = cancellation {
        cancellation.install(task.clone());
    }
    #[cfg(feature = "test-util")]
    if let Some(gate) = parts.assertion_gate {
        gate.gate()
            .update_snapshot(crate::testing::ActorDiagnostics::new(
                crate::testing::RuntimeSnapshot {
                    commit_version: *parts.commit_version,
                    live_task_ids: parts.registry.live_task_ids(),
                    cancellation_count: parts.registry.cancellation_count(),
                    timer_deadlines: parts.timers.deadlines(),
                    observation_count: parts.observations.len(),
                },
                parts.registry.cancellation_identities(),
                parts.handles.len(),
            ));
    }
    #[cfg(feature = "test-util")]
    let receipt = Some(Ok(task.clone()));
    drop(operation_work);
    #[cfg(feature = "test-util")]
    structural_terminal.record(Ok(()));
    reply.complete(task);
    Turn {
        flow: true,
        installation,
        launches,
        #[cfg(feature = "test-util")]
        receipt,
    }
}

/// Registers root effects/tasks in order without invoking their runtime factories.
pub(super) fn register_root_work<S: 'static, A: 'static, C: Execution<S, A>>(
    work: Work<EffectOf<S, A, C>, C::Task>,
    parts: &mut TransactionParts<'_, S, A, C>,
    mut origin: WorkOrigin<'_>,
    launches: &mut Launches<C::Future>,
) {
    for item in work {
        match item {
            WorkItem::Effect(effect) => parts.compile_effect(
                effect,
                WorkOrigin {
                    scope: parts.scopes.root(),
                    group: None,
                    ..origin.reborrow()
                },
                launches,
            ),
            WorkItem::Task(task) => {
                let (task, declaration) = C::task_parts(task);
                let named = declaration.map(|declaration| {
                    parts.begin_operation(
                        declaration,
                        parts.scopes.root(),
                        origin.generations,
                        origin.operations,
                    )
                });
                let operations = named
                    .as_ref()
                    .map_or(origin.operations, |(_, ancestry, _)| ancestry);
                let lineage = Arc::clone(
                    origin
                        .lineage
                        .get_or_insert_with(|| LineageState::new(Arc::downgrade(parts.control))),
                );
                let scope = parts.scopes.root();
                let prepared = parts.registry.prepare_with_operations(
                    scope,
                    Arc::clone(&lineage),
                    origin.generations,
                    operations,
                );
                let provenance = Provenance::Effect {
                    scope,
                    lineage,
                    task: prepared.sender_token(),
                    generations: prepared.generation_tokens(),
                    operations: prepared.operation_ancestry(),
                };
                let context = C::context(
                    Weak::clone(parts.ingress),
                    scope,
                    provenance,
                    parts.scopes.authority(scope),
                );
                launches.push(C::register_task(parts.registry, prepared, task, context));
                parts.end_operation(named, origin.generations);
            }
        }
    }
}

/// Dispatches typed task access; reads validate authority without making a commit.
pub(super) fn request<S: 'static, A: 'static, C: Execution<S, A>>(
    parts: TransactionParts<'_, S, A, C>,
    request: TaskRequest<S, A, C>,
    provenance: Provenance,
    authority: super::scope::ScopeAuthority,
) -> Turn<C::Future> {
    match request {
        TaskRequest::Read(call) => {
            if let Some(error) = parts.rejection(&provenance, &authority) {
                call.reject(error);
            } else {
                call.execute(parts.state);
            }
            Turn::empty(true)
        }
        TaskRequest::Update(call) => commit(parts, Mutation::Update(call, provenance, authority)),
        TaskRequest::Send(call) => commit(parts, Mutation::Send(call, provenance, authority)),
    }
}

#[cfg(test)]
#[allow(
    dead_code,
    reason = "the allocation host shares ownership fixtures but uses only their retirement controls"
)]
mod test_support;

#[cfg(all(test, feature = "allocation-measurement"))]
pub(crate) mod allocation_tests;

#[cfg(test)]
ordinary_test_items! { pub(in crate::store) mod tests; }

impl<State: 'static, Action: 'static, C: Execution<State, Action>>
    TransactionParts<'_, State, Action, C>
{
    /// Rejects closed, foreign/stale or cancelled access before user code enters.
    pub(super) fn rejection(
        &self,
        provenance: &Provenance,
        authority: &super::scope::ScopeAuthority,
    ) -> Option<TaskAccessError> {
        if let Err(error) = authority.admission() {
            return Some(error);
        }
        let reason = if !self.scopes.accepts(authority)
            || provenance
                .scope()
                .is_some_and(|scope| !self.scopes.is_current(scope))
        {
            TaskAccessReason::ScopeEnded
        } else if !provenance.is_live() {
            TaskAccessReason::Cancelled
        } else {
            return None;
        };
        Some(TaskAccessError::new(reason, TaskProgress::NotStarted))
    }

    /// Compiles one root-typed description using the common routed interpreter.
    pub(super) fn compile_effect(
        &mut self,
        effect: EffectOf<State, Action, C>,
        origin: WorkOrigin<'_>,
        launches: &mut Launches<C::Future>,
    ) {
        self.compile_routed::<Action, C::Effects, _>(effect, RootDelivery, origin, launches);
    }

    /// Registers one effect program in source order, retaining its delivery route.
    ///
    /// Cancellation and named-work brackets apply during compilation. Concrete
    /// futures are returned unpolled; concatenation retains unstarted suffixes.
    #[expect(
        clippy::too_many_lines,
        reason = "one exhaustive effect interpreter keeps generation and operation lifetime brackets and registration order visible"
    )]
    pub(super) fn compile_routed<X, E, D>(
        &mut self,
        effect: E::Effect,
        delivery: D,
        origin: WorkOrigin<'_>,
        launches: &mut Launches<C::Future>,
    ) where
        E: EffectLeaves<X>,
        D: Delivery<X, E, State, Action, C>,
    {
        let WorkOrigin {
            scope,
            lineage,
            generations,
            operations,
            inherited_generations,
            group,
        } = origin;
        let (program, effect_scope) = effect.into_parts();
        let scope = match effect_scope {
            Some(token) if !self.scopes.is_current(token) => return,
            Some(token) => token,
            None => scope,
        };
        match program {
            Program::None => {}
            Program::Dismiss(_) => crate::composition::report_unavailable_dismiss(self.issues),
            Program::Send(action) => self.register_leaf::<X, E, D>(
                &delivery,
                EffectLeaf::Send(action),
                scope,
                lineage,
                generations,
                operations,
                group,
                launches,
            ),
            Program::Run(operation) => self.register_leaf::<X, E, D>(
                &delivery,
                EffectLeaf::Run(operation),
                scope,
                lineage,
                generations,
                operations,
                group,
                launches,
            ),
            Program::FallibleRun(operation) => self.register_leaf::<X, E, D>(
                &delivery,
                EffectLeaf::Fallible(operation),
                scope,
                lineage,
                generations,
                operations,
                group,
                launches,
            ),
            Program::Future(future) => self.register_leaf::<X, E, D>(
                &delivery,
                EffectLeaf::Future(future),
                scope,
                lineage,
                generations,
                operations,
                group,
                launches,
            ),
            Program::Merge(effects) => {
                for effect in effects {
                    self.compile_routed::<X, E, D>(
                        effect,
                        delivery.clone(),
                        WorkOrigin {
                            scope,
                            lineage,
                            generations,
                            operations,
                            inherited_generations,
                            group,
                        },
                        launches,
                    );
                }
            }
            Program::Concatenate(effects) => self.register_sequence(
                delivery.sequence(effects),
                scope,
                lineage,
                generations,
                operations,
                inherited_generations,
                group,
                launches,
            ),
            Program::WithoutInheritedCancellation(effect) => {
                let mut explicit = generations[inherited_generations..].to_vec();
                self.compile_routed::<X, E, D>(
                    *effect,
                    delivery,
                    WorkOrigin {
                        scope,
                        lineage,
                        generations: &mut explicit,
                        operations,
                        inherited_generations: 0,
                        group,
                    },
                    launches,
                );
            }
            Program::Cancel(identity) => self.cancel_identity(&identity.in_scope(scope), scope),
            Program::Wait(duration) => {
                let deadline = self.now.saturating_add(duration);
                self.register_timer(
                    deadline,
                    delivery.timer(TimerKind::Wait),
                    scope,
                    lineage,
                    generations,
                    operations,
                    group,
                );
            }
            Program::SendAfter { duration, action } => {
                let deadline = self.now.saturating_add(duration);
                self.register_timer(
                    deadline,
                    delivery.timer(TimerKind::OneShot(action)),
                    scope,
                    lineage,
                    generations,
                    operations,
                    group,
                );
            }
            Program::SendAt { deadline, action } => self.register_timer(
                deadline,
                delivery.timer(TimerKind::OneShot(action)),
                scope,
                lineage,
                generations,
                operations,
                group,
            ),
            Program::Interval {
                period,
                make_action,
            } => {
                let deadline = self.now.saturating_add(period);
                self.register_timer(
                    deadline,
                    delivery.timer(TimerKind::Interval {
                        period,
                        make_action,
                    }),
                    scope,
                    lineage,
                    generations,
                    operations,
                    group,
                );
            }
            Program::Cancellable {
                effect,
                identity,
                cancel_in_flight,
            } => {
                let identity = identity.in_scope(scope);
                let cancelled_tasks = if cancel_in_flight {
                    self.request_cancellation(
                        &identity,
                        scope,
                        #[cfg(feature = "diagnostics")]
                        crate::diagnostics::CancellationCause::Replacement,
                    )
                } else {
                    Vec::new()
                };
                let token = self.registry.register_generation(identity);
                self.remove_cancelled_timers(cancelled_tasks);
                generations.push(token.clone());
                self.compile_routed::<X, E, D>(
                    *effect,
                    delivery,
                    WorkOrigin {
                        scope,
                        lineage,
                        generations,
                        operations,
                        inherited_generations,
                        group,
                    },
                    launches,
                );
                generations.pop();
                self.registry.release_generation(&token);
            }
            Program::Named {
                effect,
                declaration,
            } => {
                let named = self.begin_operation(declaration, scope, generations, operations);
                self.compile_routed::<X, E, D>(
                    *effect,
                    delivery,
                    WorkOrigin {
                        scope,
                        lineage,
                        generations,
                        operations: &named.1,
                        inherited_generations,
                        group,
                    },
                    launches,
                );
                self.end_operation(Some(named), generations);
            }
        }
    }

    /// Opens a named generation and retains registration through its owned work.
    ///
    /// Replacement cancels prior generations before publishing this generation.
    pub(super) fn begin_operation(
        &mut self,
        declaration: OperationDeclaration,
        scope: ScopeToken,
        generations: &mut Vec<GenerationToken>,
        operations: &OperationAncestry,
    ) -> (GenerationToken, OperationAncestry, OperationRegistration) {
        let completion = LineageState::new(Arc::downgrade(self.control));
        let registration = OperationRegistration::new(Arc::clone(&completion));
        let ancestry = operations.with_generation(Arc::clone(&completion));
        let identity = declaration.identity.in_scope(scope);
        let cancelled = if declaration.policy == OperationPolicy::Replace {
            self.request_cancellation(
                &identity,
                scope,
                #[cfg(feature = "diagnostics")]
                crate::diagnostics::CancellationCause::Replacement,
            )
        } else {
            Vec::new()
        };
        let token = self
            .registry
            .register_named_generation(identity, completion);
        self.remove_cancelled_timers(cancelled);
        generations.push(token.clone());
        (token, ancestry, registration)
    }

    /// Closes the matching named bracket after all enclosed work is registered.
    pub(super) fn end_operation(
        &mut self,
        named: Option<(GenerationToken, OperationAncestry, OperationRegistration)>,
        generations: &mut Vec<GenerationToken>,
    ) {
        if let Some((token, _, registration)) = named {
            generations.pop();
            self.registry.release_generation(&token);
            drop(registration);
            self.registry.prune_operations();
        }
    }

    /// Registers timer ownership before publishing a live delayed payload.
    ///
    /// A cancelled prepared timer receives a terminal signal without entering
    /// the timer queue. Group membership is established in either case.
    #[expect(
        clippy::too_many_arguments,
        reason = "explicit timer provenance and group ownership retain prepare, register and publish ordering"
    )]
    fn register_timer(
        &mut self,
        deadline: crate::MonotonicInstant,
        kind: DeferredTimer<State, Action, C>,
        scope: ScopeToken,
        lineage: &mut Option<Arc<LineageState>>,
        generations: &[GenerationToken],
        operations: &OperationAncestry,
        group: Option<&Arc<WorkGroup>>,
    ) {
        let lineage = Arc::clone(
            lineage.get_or_insert_with(|| LineageState::new(Arc::downgrade(self.control))),
        );
        let mut prepared = self.registry.prepare_with_operations(
            scope,
            Arc::clone(&lineage),
            generations,
            operations,
        );
        let task = prepared.id();
        let provenance = TimerProvenance {
            scope,
            lineage,
            task: prepared.sender_token(),
            generations: prepared.generation_tokens(),
            operations: prepared.operation_ancestry(),
        };
        if let Some(group) = group {
            if kind.is_wait() {
                let previous = self.sequence_waits.insert(group.sequence, task);
                debug_assert!(previous.is_none(), "delay group has one wait");
            }
            self.task_groups.insert(task, Arc::clone(group));
        }
        let operation_work = prepared.take_operation_work();
        self.registry.register_actor(prepared);
        if provenance.is_live() {
            let timer = self.timers.insert(
                deadline,
                TimerPayload {
                    task,
                    provenance,
                    kind,
                    operation_work,
                },
            );
            self.task_timers.insert(task, timer);
        } else {
            self.registry.complete_actor(task, TaskStatus::Cancelled);
        }
    }

    /// Cancels matching generations and releases their actor-owned timer payloads.
    fn cancel_identity(
        &mut self,
        identity: &crate::effect::CancellationIdentity,
        scope: ScopeToken,
    ) {
        let cancelled = self.request_cancellation(
            identity,
            scope,
            #[cfg(feature = "diagnostics")]
            crate::diagnostics::CancellationCause::Explicit,
        );
        self.remove_cancelled_timers(cancelled);
    }

    /// Publishes cancellation, emits diagnostics, and returns actor timers to remove.
    fn request_cancellation(
        &mut self,
        identity: &crate::effect::CancellationIdentity,
        scope: ScopeToken,
        #[cfg(feature = "diagnostics")] cause: crate::diagnostics::CancellationCause,
    ) -> Vec<TaskId> {
        #[cfg(not(feature = "diagnostics"))]
        let _ = scope;
        let cancelled = self.registry.cancel_identity(identity);
        #[cfg(feature = "diagnostics")]
        if let Some(sink) = self.lifecycle_sink.as_deref_mut() {
            crate::diagnostics::LifecycleCall::new(C::lifecycle_sink(sink), *self.commit_version)
                .emit(crate::diagnostics::LifecycleKind::CancellationRequested {
                    cause,
                    scope: Some(scope.diagnostic_id()),
                });
        }
        cancelled
    }

    /// Drops the timer payloads selected by a completed registry cancellation pass.
    fn remove_cancelled_timers(&mut self, tasks: Vec<TaskId>) {
        for task in tasks {
            self.remove_task_timer(task);
        }
    }

    /// Removes reverse indexing before destroying an owned delayed payload.
    pub(super) fn remove_task_timer(&mut self, task: TaskId) {
        if let Some(timer) = self.task_timers.remove(&task) {
            self.timers.remove(timer);
        }
    }

    /// Drops retired suffixes and completes waits that no longer guard any work.
    fn prune_deferred_scopes(&mut self) {
        for sequence in self.sequences.values_mut() {
            sequence
                .remaining
                .prune_scopes(&|scope| self.scopes.is_current(scope));
        }
        let orphaned_waits = self.sequences.iter().filter_map(|(id, sequence)| {
            if sequence.remaining.is_empty() {
                self.sequence_waits.get(id).copied()
            } else {
                None
            }
        });
        for task in orphaned_waits {
            // Only disjoint timer/registry fields change. Sequence ownership and
            // its optional wait index remain borrowed during payload destruction.
            if let Some(timer) = self.task_timers.remove(&task) {
                self.timers.remove(timer);
            }
            if self.registry.task_is_running(task) {
                self.registry.complete_actor(task, TaskStatus::Cancelled);
            }
        }
    }

    /// Registers one concrete leaf and its group membership before returning a launch.
    #[expect(
        clippy::too_many_arguments,
        reason = "explicit leaf provenance, group accounting and launch sink expose prepared-task ownership transfer"
    )]
    fn register_leaf<X, E, D>(
        &mut self,
        delivery: &D,
        leaf: EffectLeaf<X, E>,
        scope: ScopeToken,
        lineage: &mut Option<Arc<LineageState>>,
        generations: &[GenerationToken],
        operations: &OperationAncestry,
        group: Option<&Arc<WorkGroup>>,
        launches: &mut Launches<C::Future>,
    ) where
        E: EffectLeaves<X>,
        D: Delivery<X, E, State, Action, C>,
    {
        let lineage = Arc::clone(
            lineage.get_or_insert_with(|| LineageState::new(Arc::downgrade(self.control))),
        );
        let prepared = self.registry.prepare_with_operations(
            scope,
            Arc::clone(&lineage),
            generations,
            operations,
        );
        let provenance = Provenance::Effect {
            scope,
            lineage,
            task: prepared.sender_token(),
            generations: prepared.generation_tokens(),
            operations: prepared.operation_ancestry(),
        };
        self.record_task_group(prepared.id(), group);
        launches.push(delivery.register_leaf(
            self.registry,
            prepared,
            leaf,
            self.ingress,
            provenance,
        ));
    }

    /// Gives a registered task one owner of its sequence's current segment.
    fn record_task_group(&mut self, task: TaskId, group: Option<&Arc<WorkGroup>>) {
        if let Some(group) = group {
            self.task_groups.insert(task, Arc::clone(group));
        }
    }

    /// Retains a nonempty sequence's authority and suffix, then compiles its first segment.
    #[expect(
        clippy::too_many_arguments,
        reason = "deferred compilation retains the same inherited generation boundary"
    )]
    fn register_sequence(
        &mut self,
        effects: DeferredQueue<State, Action, C>,
        scope: ScopeToken,
        lineage: &mut Option<Arc<LineageState>>,
        generations: &[GenerationToken],
        operations: &OperationAncestry,
        inherited_generations: usize,
        parent_group: Option<&Arc<WorkGroup>>,
        launches: &mut Launches<C::Future>,
    ) {
        if effects.is_empty() {
            return;
        }

        let lineage = Arc::clone(
            lineage.get_or_insert_with(|| LineageState::new(Arc::downgrade(self.control))),
        );
        for token in generations {
            self.registry.retain_generation(token);
        }
        let sequence_id = *self.next_sequence_id;
        *self.next_sequence_id = self
            .next_sequence_id
            .checked_add(1)
            .expect("effect sequence ID space exhausted");
        let work = lineage.add_work();
        self.sequences.insert(
            sequence_id,
            Sequence {
                remaining: effects,
                scope,
                lineage,
                generations: generations.into(),
                inherited_generations,
                parent_group: parent_group.cloned(),
                _work: work,
                operations: operations.clone(),
                _operation_work: operations.retain_work(),
            },
        );
        self.advance_sequence(
            Arc::new(WorkGroup {
                sequence: sequence_id,
                cancelled: AtomicBool::new(false),
            }),
            launches,
        );
    }

    /// Compiles empty segments synchronously until one owns work or the sequence ends.
    ///
    /// Liveness is checked before popping a suffix. The registration owner keeps
    /// nested synchronous completion from advancing this sequence during compilation.
    fn advance_sequence(&mut self, mut group: Arc<WorkGroup>, launches: &mut Launches<C::Future>) {
        let sequence_id = group.sequence;
        loop {
            let (effect, scope, lineage, mut generations, operations, inherited_generations) = {
                let sequence = self
                    .sequences
                    .get_mut(&sequence_id)
                    .expect("active sequence must exist");
                if sequence.lineage.is_cancelled()
                    || !self.scopes.is_current(sequence.scope)
                    || sequence.generations.iter().any(|token| !token.is_live())
                {
                    self.finish_sequence(sequence_id, true, launches);
                    return;
                }
                let Some(effect) = sequence.remaining.pop_front() else {
                    self.finish_sequence(sequence_id, false, launches);
                    return;
                };
                (
                    effect,
                    sequence.scope,
                    Arc::clone(&sequence.lineage),
                    sequence.generations.to_vec(),
                    sequence.operations.clone(),
                    sequence.inherited_generations,
                )
            };

            let mut active_lineage = Some(lineage);
            match effect {
                DeferredEffect::Root(effect) => self.compile_effect(
                    effect,
                    WorkOrigin {
                        scope,
                        lineage: &mut active_lineage,
                        generations: &mut generations,
                        operations: &operations,
                        inherited_generations,
                        group: Some(&group),
                    },
                    launches,
                ),
                DeferredEffect::Routed(effect) => effect.resume(
                    self,
                    WorkOrigin {
                        scope,
                        lineage: &mut active_lineage,
                        generations: &mut generations,
                        operations: &operations,
                        inherited_generations,
                        group: Some(&group),
                    },
                    launches,
                ),
            }

            if Arc::get_mut(&mut group).is_none() {
                return;
            }
            self.sequence_waits.remove(&sequence_id);
            if group.cancelled.load(Ordering::Relaxed) {
                self.finish_sequence(sequence_id, true, launches);
                return;
            }
        }
    }

    /// Releases timer indexing and accounts for one harvested group member.
    pub(super) fn complete_task(&mut self, task: TerminalTask, launches: &mut Launches<C::Future>) {
        self.remove_task_timer(task.id);
        let Some(group) = self.task_groups.remove(&task.id) else {
            return;
        };
        self.complete_group_member(group, task.status != TaskStatus::Completed, launches);
    }

    /// Advances or cancels a segment when explicit completion owns its final token.
    fn complete_group_member(
        &mut self,
        mut group: Arc<WorkGroup>,
        cancelled: bool,
        launches: &mut Launches<C::Future>,
    ) {
        if cancelled {
            group.cancelled.store(true, Ordering::Relaxed);
        }
        if Arc::get_mut(&mut group).is_none() {
            return;
        }

        self.sequence_waits.remove(&group.sequence);
        if group.cancelled.load(Ordering::Relaxed) {
            self.finish_sequence(group.sequence, true, launches);
        } else {
            self.advance_sequence(group, launches);
        }
    }

    /// Drops a sequence's remaining captures and leases before completing its parent.
    fn finish_sequence(
        &mut self,
        sequence_id: SequenceId,
        cancelled: bool,
        launches: &mut Launches<C::Future>,
    ) {
        let mut sequence = self
            .sequences
            .remove(&sequence_id)
            .expect("terminal sequence must exist");
        for token in &sequence.generations {
            self.registry.release_generation(token);
        }
        let parent_group = sequence.parent_group.take();
        drop(sequence);
        if let Some(group) = parent_group {
            self.complete_group_member(group, cancelled, launches);
        }
    }
}

/// Handles ordered named-work access without mutating state or advancing revision.
pub(super) fn operation<S: 'static, A: 'static, C: Execution<S, A>>(
    mut parts: TransactionParts<'_, S, A, C>,
    request: super::operation::OperationRequest,
    metadata: &(impl super::operation::OperationMetadata + ?Sized),
) -> Turn<C::Future> {
    use super::operation::{OperationRequest, OperationTarget};
    let target = metadata.target();
    if let Err(error) = target.admission() {
        request.reject(error);
        return Turn::empty(true);
    }
    let scope = match target {
        // Root endpoints privately pair ingress and lifecycle from one Store.
        // They cannot be retargeted to another actor by public code.
        OperationTarget::Root { .. } => parts.scopes.root(),
        OperationTarget::Frozen { scope, authority } => {
            if let Some(error) = parts.rejection(metadata.provenance(), authority) {
                request.reject(error);
                return Turn::empty(true);
            }
            *scope
        }
    };
    let identity = metadata.identity().clone().in_scope(scope);
    match request {
        OperationRequest::Status(reply) => {
            reply.complete(Ok(parts.registry.operation_status(&identity)));
        }
        OperationRequest::Capture(reply) => {
            reply.complete(Ok(parts.registry.capture_operation(&identity)));
        }
        OperationRequest::Cancel(reply) => {
            parts.cancel_identity(&identity, scope);
            parts.registry.prune_operations();
            reply.complete(Ok(()));
        }
    }
    Turn::empty(true)
}

/// Attempts Shared effect admission now, boxing only the full-mailbox wait path.
pub(super) fn send_effect_action<State: Send + 'static, Action: Send + 'static>(
    ingress: &Weak<Ingress<State, Action>>,
    provenance: &Provenance,
    action: Action,
) -> EffectSendFuture {
    match admit_effect_action::<State, Action, Shared>(ingress, provenance, action) {
        EffectAdmission::Receiving(receive) => EffectSendFuture::Receiving(receive),
        EffectAdmission::Cancelled => EffectSendFuture::Cancelled,
        EffectAdmission::Stopped => EffectSendFuture::StoreStopped,
        EffectAdmission::Full {
            ingress,
            command,
            receive,
        } => EffectSendFuture::Admitting(Box::pin(async move {
            ingress
                .send(command)
                .await
                .map_err(|_closed| EffectSendError::store_stopped())?;
            drop(ingress);
            Ok(receive)
        })),
    }
}

/// Separates eager effect admission from later capacity and commit waits.
pub(super) enum EffectAdmission<S, A, C: Carriers<S, A>> {
    /// The mailbox owns the action; only its commit acknowledgement remains.
    Receiving(super::command::CommitReceiver),
    /// Capacity is unavailable, so the caller still owns the unadmitted command.
    Full {
        /// Keeps admission alive only until the queued send resolves.
        ingress: Arc<CommandIngress<Command<S, A, C>>>,
        /// Owns the original action and named-work leases until transfer or drop.
        command: Command<S, A, C>,
        /// Cannot contain the original action; it carries only commit completion.
        receive: super::command::CommitReceiver,
    },
    /// Causal or named-work authority expired before admission.
    Cancelled,
    /// Ingress ownership or mailbox admission has closed.
    Stopped,
}

/// Validates, retains causal work, rechecks cancellation, then tries typed admission.
///
/// Both liveness checks are needed: cancellation can race lease retention. On
/// failure the original action is destroyed before its retained operation work.
pub(super) fn admit_effect_action<S, A, C: Carriers<S, A>>(
    ingress: &Weak<CommandIngress<Command<S, A, C>>>,
    provenance: &Provenance,
    action: A,
) -> EffectAdmission<S, A, C> {
    if !provenance.is_live() {
        return EffectAdmission::Cancelled;
    }
    let Some(ingress) = ingress.upgrade() else {
        return EffectAdmission::Stopped;
    };
    let operation_work = match provenance.operations().try_retain_work() {
        Ok(work) => work,
        Err((_, partial)) => {
            drop(action);
            drop(partial);
            return EffectAdmission::Cancelled;
        }
    };
    #[cfg(all(test, feature = "test-util"))]
    ordinary_test_items! {
        super::mounted::mounted_admission_test_hook::after_retain();
    }
    if !provenance.is_live() {
        drop(action);
        drop(operation_work);
        return EffectAdmission::Cancelled;
    }
    let (reply, receive) = crate::store::command::commit_channel();
    let command = Command::Action(ActionCommand {
        input: super::command::ActionInput::Ready {
            action,
            provenance: provenance.clone(),
        },
        reply,
        #[cfg(feature = "test-util")]
        assertion_reply: None,
        operation_work,
    });
    match ingress.try_send(command) {
        Ok(()) => EffectAdmission::Receiving(receive),
        Err(async_channel::TrySendError::Closed(_command)) => EffectAdmission::Stopped,
        Err(async_channel::TrySendError::Full(command)) => EffectAdmission::Full {
            ingress,
            command,
            receive,
        },
    }
}
