//! Selects shared or thread-local types for the common transaction kernel.
//!
//! `Carriers` keeps the related callback, command and task types together.
//! `Execution` supplies operations that need their concrete runtime types.
//! The kernel uses these traits through generics, not trait objects.
//!
//! Shared callbacks and futures can move between threads. Local equivalents can
//! retain `Rc` values and other non-`Send` captures on their owning thread.
//! The common bounds must not require `Send` from both families.

#[cfg(all(test, feature = "allocation-measurement", feature = "local"))]
pub(crate) mod allocation_tests;

use super::command::{Command, Provenance};
use super::ingress::CommandIngress;
use super::task_context::{CommitReply, ReadCall, ReadInput, SharedReply, UpdateCall};
use crate::effect::leaves::EffectLeaves;
use crate::effect::registry::{PreparedTask, TaskRegistry};
use crate::feature::ReduceDriver;
use std::sync::Weak;

/// Selects one compatible family of owned commands, callbacks and runtime values.
///
/// Associated types keep `Send` requirements at the concrete shared boundary.
/// `?Sized` permits each family to select its own erased callback type.
/// The family itself is sized because commands refer back to their carrier.
pub(crate) trait Carriers<S, A>: Sized {
    /// A runtime-owned future that the actor can poll through a mutable reference.
    type Future: Future<Output = ()> + Unpin + 'static;
    /// A clonable handle to the selected runtime.
    type Runtime: Clone + 'static;
    /// A query from the original store-read interface.
    type LegacyRead: ?Sized + super::read::ReadCommand<S>;
    /// A query that validates its exact child selection before projection.
    type CheckedRead: ?Sized + super::read::CheckedReadCommand<S>;
    /// Selects the current identity for a scoped store handle.
    type HandleSelector: ?Sized + super::scope::HandleSelector<S>;
    /// A retained observation with its family-specific capture bounds.
    type ObservationEntry: ?Sized + super::observation::ErasedObservation<S>;
    /// An owned observation registration, interpreted by `Execution::observe`.
    type ObservationCommand: ?Sized;
    /// A typed task endpoint with frozen scope and producer authority.
    type Context;
    /// Selects the owned leaves within the common effect program.
    type Effects: EffectLeaves<A>;
    /// An owned task whose body starts after transaction registration.
    type Task;
    /// Creates optional initial work from a borrowed root state.
    type RootInitialCallback: for<'a> FnMut(&S, crate::ReduceContext<'a>) -> Option<Self::Task>;
    /// Owns a root reaction that can capture state before a mutation.
    type RootReaction: std::ops::Deref<Target: super::reaction::Hook<S, Self::Task>>;
    /// Selects the root configuration marker without replacing its identity.
    type RootConfiguration: FnMut(&S) -> u64;
    /// Reduces actions into effects and tasks from this same family.
    type Driver: ?Sized
        + ReduceDriver<S, A, Effect = <Self::Effects as EffectLeaves<A>>::Effect, Task = Self::Task>;
    /// A query ready for execution against actor-owned state.
    type Read: ?Sized + ReadCall<S>;
    /// An input query that can record entry before endpoint projection.
    type ReadInput: ?Sized + ReadInput<S>;

    /// Forgets the input-only interface without allocating or changing ownership.
    fn into_read(input: Box<Self::ReadInput>) -> Box<Self::Read>;

    /// An owned mutation whose reply matches the selected transaction family.
    type Update: ?Sized + UpdateCall<S, Reply = Self::Reply>;
    /// Completes a mutation after the common transaction commits.
    type Reply: CommitReply;
    /// Receives the final state after store shutdown.
    type Terminal: ?Sized + FnOnce(S);
    /// Receives synchronous actor lifecycle diagnostics.
    #[cfg(feature = "diagnostics")]
    type LifecycleSink: ?Sized + FnMut(&crate::diagnostics::LifecycleEvent);

    /// Borrows a diagnostic callback without its family-specific thread bound.
    #[cfg(feature = "diagnostics")]
    fn lifecycle_sink(
        sink: &mut Self::LifecycleSink,
    ) -> &mut dyn FnMut(&crate::diagnostics::LifecycleEvent);

    /// Reads state synchronously for test assertions.
    #[cfg(feature = "test-util")]
    type Query: ?Sized + FnOnce(&S);
    /// Weakly identifies a test output route without retaining its owner.
    #[cfg(feature = "test-util")]
    type OutputAnchor;
    /// Executes a typed request against an installed child.
    type Mounted: ?Sized + super::mounted::MountedCall<S, A, Self>;
    /// Routes a typed child output through its parent transaction.
    type Output: ?Sized + super::output::OutputCall<S, A, Self>;
    /// Owns work produced by an installed child.
    type MountedWork: ?Sized + super::mounted::MountedWork<S, A, Self>;
    /// Retains child effect sequences until the kernel can advance them.
    type Deferred: ?Sized + super::mounted::DeferredProgram<S, A, Self>;
    /// Creates routed child actions for actor-owned timers.
    type Timer: ?Sized + super::mounted::RoutedTimer<S, A, Self>;
    /// Retains an installed feature and exposes its scope identity.
    type Node: ?Sized + super::mounted::InstalledNode;
    /// Resolves an acquisition request against the current mount installation.
    type Acquire: ?Sized + super::mount::AcquireCall;
    /// Reads ordered contributions from the committed child roster.
    type Fold: ?Sized + super::fold::FoldCall<S, A, Self>;
    /// Describes child discovery, installation and reconciliation.
    type Mount: ?Sized + super::mount::MountDeclaration<S, A, Self>;
    /// Shares immutable metadata for a named-operation request.
    type OperationMetadata: std::ops::Deref<Target: super::operation::OperationMetadata>;
}

/// The effect value selected by a store's carrier family.
pub(crate) type EffectOf<S, A, C> = <<C as Carriers<S, A>>::Effects as EffectLeaves<A>>::Effect;
/// The action factory selected for an actor-owned interval.
pub(crate) type IntervalOf<S, A, C> = <<C as Carriers<S, A>>::Effects as EffectLeaves<A>>::Interval;

/// Owns one effect body that needs runtime execution.
///
/// The transaction kernel handles sequencing, cancellation and timers separately.
pub(crate) enum EffectLeaf<A, E: EffectLeaves<A>> {
    /// Sends one owned action and waits for its commit.
    Send(A),
    /// Starts an operation that can send causal actions.
    Run(E::Run),
    /// Starts an operation whose failure routing needs cancellation authority.
    Fallible(E::FallibleRun),
    /// Awaits an optional action, then sends it if present.
    Future(E::Future),
}

/// Adapts concrete runtime operations to the common transaction kernel.
///
/// These associated functions resolve statically for `Shared` or `Local`.
/// Registration returns an unpolled future. The actor launches it after commit.
pub(crate) trait Execution<S, A>: Carriers<S, A> {
    /// Reads the selected runtime's monotonic clock.
    fn now(runtime: &Self::Runtime) -> crate::MonotonicInstant;
    /// Gives other runtime tasks an opportunity to run.
    fn yield_now(runtime: &Self::Runtime) -> Self::Future;
    /// Creates a wait for an absolute runtime deadline.
    fn sleep_until(runtime: &Self::Runtime, deadline: crate::MonotonicInstant) -> Self::Future;
    /// Transfers an owned future to the selected runtime.
    fn spawn(runtime: &Self::Runtime, future: Self::Future);
    /// Installs an observation against the current state and scope.
    fn observe(
        command: Box<Self::ObservationCommand>,
        state: &S,
        registry: &mut super::observation::ObservationRegistryCore<S, Self::ObservationEntry>,
        version: u64,
        scope: super::scope::ScopeToken,
        handle: Option<super::scope::WeakHandleScope>,
        validation: &super::scope::SelectedChild<'_>,
    );
    /// Separates optional named-operation metadata from the owned task body.
    fn task_parts(
        task: Self::Task,
    ) -> (
        Self::Task,
        Option<crate::effect::operation::OperationDeclaration>,
    );
    /// Creates a task endpoint without retaining the store's command ingress.
    fn context(
        ingress: Weak<CommandIngress<Command<S, A, Self>>>,
        scope: super::scope::ScopeToken,
        provenance: Provenance,
        authority: super::scope::ScopeAuthority,
    ) -> Self::Context;
    /// Registers an owned task without polling its user body.
    fn register_task(
        registry: &mut TaskRegistry,
        prepared: PreparedTask,
        task: Self::Task,
        context: Self::Context,
    ) -> Self::Future;
    /// Registers an effect body with its captured producer authority.
    fn register_effect(
        registry: &mut TaskRegistry,
        prepared: PreparedTask,
        leaf: EffectLeaf<A, Self::Effects>,
        ingress: Weak<CommandIngress<Command<S, A, Self>>>,
        provenance: Provenance,
    ) -> Self::Future;
}

impl<S: Send + 'static, A: Send + 'static> Execution<S, A> for Shared {
    fn now(runtime: &Self::Runtime) -> crate::MonotonicInstant {
        runtime.now()
    }

    fn yield_now(runtime: &Self::Runtime) -> Self::Future {
        runtime.yield_now()
    }

    fn sleep_until(runtime: &Self::Runtime, deadline: crate::MonotonicInstant) -> Self::Future {
        runtime.sleep_until(deadline)
    }

    fn spawn(runtime: &Self::Runtime, future: Self::Future) {
        runtime.spawn(future);
    }

    fn observe(
        command: Box<Self::ObservationCommand>,
        state: &S,
        registry: &mut super::observation::ObservationRegistryCore<S, Self::ObservationEntry>,
        version: u64,
        scope: super::scope::ScopeToken,
        handle: Option<super::scope::WeakHandleScope>,
        validation: &super::scope::SelectedChild<'_>,
    ) {
        command.execute(state, registry, version, scope, handle, validation);
    }

    fn task_parts(
        task: Self::Task,
    ) -> (
        Self::Task,
        Option<crate::effect::operation::OperationDeclaration>,
    ) {
        task.into_parts()
    }

    fn context(
        ingress: Weak<CommandIngress<Command<S, A>>>,
        scope: super::scope::ScopeToken,
        provenance: Provenance,
        authority: super::scope::ScopeAuthority,
    ) -> Self::Context {
        crate::TaskContext::new(ingress, scope, provenance, authority)
    }

    fn register_task(
        registry: &mut TaskRegistry,
        prepared: PreparedTask,
        task: Self::Task,
        context: Self::Context,
    ) -> Self::Future {
        registry.register(prepared, task.start(context))
    }

    fn register_effect(
        registry: &mut TaskRegistry,
        prepared: PreparedTask,
        leaf: EffectLeaf<A, Self::Effects>,
        ingress: Weak<CommandIngress<Command<S, A>>>,
        provenance: Provenance,
    ) -> Self::Future {
        let sender = move |authority| {
            move |action| super::transaction::send_effect_action(&ingress, &authority, action)
        };

        match leaf {
            EffectLeaf::Send(action) => {
                let send = sender(provenance);
                registry.register(prepared, async move {
                    let _ = send(action).await;
                })
            }
            EffectLeaf::Run(operation) => {
                let sender = crate::EffectSender::new(sender(provenance));
                registry.register(prepared, async move {
                    operation(sender).await;
                })
            }
            EffectLeaf::Fallible(operation) => {
                // Sending and failure routing each retain the same authority.
                let sender = crate::EffectSender::new(sender(provenance.clone()));
                registry.register_with_cancellation(prepared, move |cancellation| async move {
                    operation(sender, provenance, cancellation).await;
                })
            }
            EffectLeaf::Future(future) => {
                let send = sender(provenance);
                registry.register(prepared, async move {
                    if let Some(action) = future.await {
                        let _ = send(action).await;
                    }
                })
            }
        }
    }
}

/// Selects callbacks and futures that can move between runtime threads.
pub(crate) struct Shared;

impl<S, A> Carriers<S, A> for Shared {
    #[cfg(feature = "test-util")]
    type OutputAnchor = std::sync::Weak<dyn std::any::Any + Send + Sync>;
    type Runtime = crate::Runtime;
    type LegacyRead = dyn super::read::ReadCommand<S> + Send;
    type CheckedRead = dyn super::read::CheckedReadCommand<S> + Send;
    type HandleSelector = dyn super::scope::HandleSelector<S> + Send;
    type ObservationEntry = dyn super::observation::ErasedObservation<S> + Send;
    type ObservationCommand = dyn super::observation::ObservationCommand<S>;
    type OperationMetadata = std::sync::Arc<dyn super::operation::OperationMetadata + Send + Sync>;
    type Acquire = dyn super::mount::AcquireCall + Send;
    type Fold = dyn super::fold::FoldCall<S, A, Self> + Send;
    type Mount = dyn super::mount::MountDeclaration<S, A> + Send;
    type Future = crate::RuntimeFuture;
    type Context = crate::TaskContext<S, A>;
    type Effects = crate::effect::leaves::SharedEffects;
    type Task = crate::Task<S, A>;
    type RootInitialCallback =
        Box<dyn for<'a> FnMut(&S, crate::ReduceContext<'a>) -> Option<Self::Task> + Send>;
    type RootReaction = super::reaction::SharedHook<S, A>;
    type RootConfiguration = Box<dyn FnMut(&S) -> u64 + Send>;
    type Driver =
        dyn ReduceDriver<S, A, Effect = crate::Effect<A>, Task = crate::Task<S, A>> + Send;
    type Read = dyn ReadCall<S> + Send;
    type ReadInput = dyn ReadInput<S> + Send;

    fn into_read(input: Box<Self::ReadInput>) -> Box<Self::Read> {
        input
    }

    type Update = dyn UpdateCall<S, Reply = SharedReply> + Send;
    type Reply = SharedReply;
    type Terminal = dyn FnOnce(S) + Send;
    #[cfg(feature = "diagnostics")]
    type LifecycleSink = dyn FnMut(&crate::diagnostics::LifecycleEvent) + Send;

    #[cfg(feature = "diagnostics")]
    fn lifecycle_sink(
        sink: &mut Self::LifecycleSink,
    ) -> &mut dyn FnMut(&crate::diagnostics::LifecycleEvent) {
        sink
    }

    #[cfg(feature = "test-util")]
    type Query = dyn FnOnce(&S) + Send;
    type Mounted = dyn super::mounted::MountedCall<S, A, Self> + Send;
    type Output = dyn super::output::OutputCall<S, A, Self> + Send;
    type MountedWork = dyn super::mounted::MountedWork<S, A, Self> + Send;
    type Deferred = dyn super::mounted::DeferredProgram<S, A, Self> + Send;
    type Timer = dyn super::mounted::RoutedTimer<S, A, Self> + Send;
    type Node = dyn super::mounted::InstalledNode + Send;
}
