//! Owns serial feature mutation, effect registration and terminal teardown.
//!
//! Commands retain admission order. Bounded command and timer batches alternate
//! when both are ready; control signals are harvested before either source.
//! Registration prepares unpolled work, and launching hands that work to the
//! selected Shared or Local runtime only after the transaction has committed.
//!
//! Explicit shutdown releases actor-owned resources before acknowledging success.
//! Unwinding instead drops pending commands through their failure guards. A second
//! panic from a destructor or waker during unwinding follows Rust's abort behaviour.

use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Weak};

use futures_util::future::{Either, FutureExt, select};

#[cfg(test)]
use super::Ingress;
use super::carriers::{Carriers, Execution, Shared};
use super::command::{ActionCommand, Command, CommitError, TerminalReply};
#[cfg(feature = "test-util")]
use super::command::{ActionInput, ReadyActionCommand};
use super::fold::FoldCall as _;
use super::mount::AcquireCall as _;
use super::mount::MountDeclaration as _;
#[cfg(feature = "test-util")]
use super::mounted::MountedCall as _;
use super::mounted::RoutedTimer as _;
use super::observation::ObservationRegistryCore;
use super::read::{CheckedReadCommand as _, ReadCommand as _};
use super::scope::{HandleRegistryCore, ScopeRegistry, ScopeToken};
use super::transaction::{Mutation, Turn};
use super::{CloseState, Lifecycle};
#[cfg(test)]
use crate::ReduceContext;
#[cfg(test)]
use crate::Runtime;
#[cfg(test)]
use crate::effect::Effect;
#[cfg(test)]
use crate::effect::Program;
use crate::effect::lineage::{ControlWake, LineageState, LineageWork};
use crate::effect::registry::{GenerationToken, TaskCompletion, TaskId, TaskRegistry, TaskStatus};
use crate::effect::timer::{TimerId, TimerQueue};
#[cfg(test)]
use crate::feature::ReduceDriver;
#[cfg(test)]
use crate::reducer::ErasedReducer;
use crate::{DependencyValues, RuntimeFuture};

#[cfg(test)]
mod test_support;

#[cfg(all(test, feature = "allocation-measurement"))]
pub(crate) mod allocation_tests;

/// Maximum ready commands, or timer-free turns, before giving other work a turn.
const COMMAND_BATCH_LIMIT: usize = 64;
/// Maximum due timers consumed before command arbitration resumes.
const TIMER_BATCH_LIMIT: usize = 64;

/// Ordered, unpolled work prepared by one actor turn, with its first item inline.
pub(crate) struct Launches<F = RuntimeFuture> {
    /// Earliest launch; keeping it inline avoids a buffer for a single task.
    first: Option<F>,
    /// Later launches in registration order; drops after the first launch.
    rest: Vec<F>,
}

impl<F> Default for Launches<F> {
    fn default() -> Self {
        Self {
            first: None,
            rest: Vec::new(),
        }
    }
}

impl<F> Launches<F> {
    /// Appends work without polling it or transferring it to the runtime.
    pub(super) fn push(&mut self, future: F) {
        if self.first.is_none() {
            self.first = Some(future);
        } else {
            self.rest.push(future);
        }
    }
}

#[cfg(all(test, not(loom)))]
ordinary_test_items! {
    mod launch_tests {
        use std::pin::Pin;
        use std::sync::{Arc, Mutex};
        use std::task::{Context, Poll};

        use super::Launches;
        use crate::RuntimeFuture;

        /// Records lazy polling and ordered destruction without invoking a host executor.
        struct Launch {
            id: usize,
            dropped: Arc<Mutex<Vec<usize>>>,
            polled: Arc<Mutex<Vec<usize>>>,
        }

        impl Future for Launch {
            type Output = ();

            fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<()> {
                self.polled.lock().unwrap().push(self.id);
                Poll::Ready(())
            }
        }

        impl Drop for Launch {
            fn drop(&mut self) {
                self.dropped.lock().unwrap().push(self.id);
            }
        }

        /// Builds unpolled captures whose identities expose any reorder during buffer growth.
        fn batch(
            count: usize,
            dropped: &Arc<Mutex<Vec<usize>>>,
            polled: &Arc<Mutex<Vec<usize>>>,
        ) -> Launches {
            let mut launches: Launches = Launches::default();
            for id in 0..count {
                launches.push(Box::pin(Launch {
                    id,
                    dropped: Arc::clone(dropped),
                    polled: Arc::clone(polled),
                }));
            }
            launches
        }

        #[test]
        fn launches_stay_lazy_and_preserve_order_across_promotion() {
            for count in [0, 1, 2, 5] {
                let dropped = Arc::new(Mutex::new(Vec::new()));
                let polled = Arc::new(Mutex::new(Vec::new()));
                let launches = batch(count, &dropped, &polled);
                assert!(polled.lock().unwrap().is_empty());
                for launch in launches {
                    futures_executor::block_on(launch);
                }
                let expected: Vec<_> = (0..count).collect();
                assert_eq!(*polled.lock().unwrap(), expected);
                assert_eq!(*dropped.lock().unwrap(), expected);
            }
        }

        #[test]
        fn unspawned_launches_drop_in_order_without_polling() {
            for count in [0, 1, 2, 5] {
                let dropped = Arc::new(Mutex::new(Vec::new()));
                let polled = Arc::new(Mutex::new(Vec::new()));
                drop(batch(count, &dropped, &polled));
                assert!(polled.lock().unwrap().is_empty());
                assert_eq!(*dropped.lock().unwrap(), (0..count).collect::<Vec<_>>());
            }
        }

        #[test]
        fn spawn_panic_drops_remaining_launches_without_polling() {
            for count in [1, 2, 5] {
                let dropped = Arc::new(Mutex::new(Vec::new()));
                let polled = Arc::new(Mutex::new(Vec::new()));
                let launches = batch(count, &dropped, &polled);
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    for launch in launches {
                        panic_on_spawn(launch);
                    }
                }));
                result.expect_err("the host spawn panics");
                assert!(polled.lock().unwrap().is_empty());
                assert_eq!(*dropped.lock().unwrap(), (0..count).collect::<Vec<_>>());
            }
        }

        /// Takes ownership exactly where a host spawn failure would destroy its argument.
        fn panic_on_spawn(_launch: RuntimeFuture) {
            panic!("host spawn failure");
        }
    }
}

impl<F> IntoIterator for Launches<F> {
    type Item = F;
    type IntoIter = std::iter::Chain<std::option::IntoIter<F>, std::vec::IntoIter<F>>;

    fn into_iter(self) -> Self::IntoIter {
        self.first.into_iter().chain(self.rest)
    }
}

/// Sole owner of mutable feature state and its serial command-processing protocol.
///
/// `C` selects Shared or Local callbacks and futures without changing ordering.
/// Field order keeps lifecycle acknowledgement after feature and runtime cleanup;
/// test gates remain bound while earlier user-owned fields are destroyed.
pub(crate) struct Actor<State, Action, C: Carriers<State, Action> = Shared> {
    /// Live state, taken only when successful shutdown returns it to the caller.
    state: Option<State>,
    /// Feature driver invoked only by actor-owned transactions.
    reducer: Box<C::Driver>,
    /// Dependency values inherited by transactions and the work they register.
    dependencies: DependencyValues,
    /// Actor-owned issue endpoint, closed after actor resources are released.
    issues: crate::IssueReporter,
    /// Optional serial lifecycle observer, retained through terminal notifications.
    #[cfg(feature = "diagnostics")]
    lifecycle_sink: Option<Box<C::LifecycleSink>>,
    /// Sole consumer of admitted commands; closed and drained before teardown.
    receiver: super::mailbox::Receiver<Command<State, Action, C>>,
    /// Submission endpoint without an owner that could keep the store open.
    ingress: Weak<super::ingress::CommandIngress<Command<State, Action, C>>>,
    /// Host executor and clock, with thread affinity selected by `C`.
    runtime: C::Runtime,
    /// Running work, cancellation generations and terminal control notifications.
    registry: TaskRegistry,
    /// Live structural lifetimes and the authority accepted for each lifetime.
    scopes: ScopeRegistry,
    /// Installed feature nodes, in the ordering used for structural traversal.
    mounted: Vec<Box<C::Node>>,
    /// Mount declarations used to bind and discover feature nodes.
    mounts: Vec<Box<C::Mount>>,
    /// Whether any declaration needs discovery after a state mutation.
    has_mount_recipes: bool,
    /// Scoped projection handles whose identity must survive the next transaction.
    handles: HandleRegistryCore<C::HandleSelector>,
    /// Shared notification source for admission, cancellation and task completion.
    control: Arc<ControlWake>,
    /// Last harvested notification epoch, checked after registering each listener.
    control_epoch: u64,
    /// Monotonic version published by committed state transactions.
    commit_version: u64,
    /// Reverse index from an unfinished task to the sequence segment awaiting it.
    task_groups: HashMap<TaskId, Arc<WorkGroup>>,
    /// Active delay tasks whose sequence suffix can disappear during pruning.
    sequence_waits: HashMap<SequenceId, TaskId>,
    /// Suspended effect sequences and the work they retain until continuation.
    sequences: HashMap<SequenceId, Sequence<super::mounted::DeferredQueue<State, Action, C>>>,
    /// Next checked sequence identity; identities are not reused.
    next_sequence_id: SequenceId,
    /// Actor-owned deadlines and their unexecuted actions or routed callbacks.
    timers: TimerQueue<TimerPayload<State, Action, C>>,
    /// Reverse index used to remove a cancelled task's current timer entry.
    task_timers: HashMap<TaskId, TimerId>,
    /// Terminal reply withheld until teardown and any terminal callbacks succeed.
    shutdown_reply: Option<TerminalReply<C::Terminal>>,
    /// Installed projections notified after committed mutations.
    observations: ObservationRegistryCore<State, C::ObservationEntry>,
    /// Publishes terminal lifecycle state after earlier fields are destroyed.
    #[cfg(not(feature = "diagnostics"))]
    guard: ActorGuard,
    /// Movable terminal guard, held past diagnostic callbacks during finalisation.
    #[cfg(feature = "diagnostics")]
    guard: Option<ActorGuard>,
    #[cfg(all(test, feature = "test-util"))]
    before_cancelled_gate_skip: Option<std::pin::Pin<Box<dyn Future<Output = ()> + Send + Sync>>>,
    #[cfg(all(test, feature = "test-util"))]
    before_gate_listen: Option<Box<dyn FnOnce() + Send + Sync>>,
    /// Inserts a test notification before timer-free listener registration.
    #[cfg(test)]
    before_untimed_listener: Option<Box<dyn FnOnce() + Send + Sync>>,
    /// Inserts test admission or closure after the final timer-free epoch check.
    #[cfg(test)]
    before_untimed_recheck: Option<Box<dyn FnOnce() + Send + Sync>>,
    /// Legacy effect gate, dropped after user-owned fields that may rebind it.
    #[cfg(feature = "test-util")]
    assertion_gate: Option<crate::testing::AssertionGateBinding<Action>>,
    /// Typed transaction frontier, retained through all earlier user destructors.
    #[cfg(feature = "test-util")]
    transaction_gate: Option<crate::testing::TransactionGateBinding<State, Action, C>>,
}

#[cfg(test)]
impl<State, Action> Actor<State, Action>
where
    State: Send + 'static,
    Action: Send + 'static,
{
    #[expect(
        clippy::too_many_arguments,
        reason = "the actor constructor makes every owned runtime component explicit"
    )]
    #[cfg(test)]
    /// Adapts legacy reducer fixtures to the production driver and ownership protocol.
    fn new(
        state: State,
        reducer: Box<dyn ErasedReducer<State, Action>>,
        dependencies: DependencyValues,
        issues: crate::IssueReporter,
        receiver: super::mailbox::Receiver<Command<State, Action>>,
        ingress: &Arc<Ingress<State, Action>>,
        lifecycle: Arc<Lifecycle>,
        runtime: Runtime,
        #[cfg(feature = "test-util")] assertion_gate: Option<
            crate::testing::AssertionGateBinding<Action>,
        >,
    ) -> Self {
        Self::with_driver(
            state,
            Box::new(LegacyDriver(reducer)),
            dependencies,
            issues,
            receiver,
            ingress,
            lifecycle,
            runtime,
            #[cfg(feature = "test-util")]
            assertion_gate,
        )
    }
}

impl<State: 'static, Action: 'static, C: Execution<State, Action> + 'static>
    Actor<State, Action, C>
{
    /// Binds actor-owned resources without invoking features or spawning work.
    #[expect(
        clippy::too_many_arguments,
        reason = "the actor constructor makes every owned runtime component explicit"
    )]
    pub(crate) fn with_driver(
        state: State,
        reducer: Box<C::Driver>,
        dependencies: DependencyValues,
        issues: crate::IssueReporter,
        receiver: super::mailbox::Receiver<Command<State, Action, C>>,
        ingress: &Arc<super::ingress::CommandIngress<Command<State, Action, C>>>,
        lifecycle: Arc<Lifecycle>,
        runtime: C::Runtime,
        #[cfg(feature = "test-util")] assertion_gate: Option<
            crate::testing::AssertionGateBinding<Action>,
        >,
    ) -> Self {
        let control = Arc::clone(&ingress.control);
        let scopes = ScopeRegistry::new(&lifecycle);
        Self {
            state: Some(state),
            reducer,
            dependencies,
            issues,
            #[cfg(feature = "diagnostics")]
            lifecycle_sink: None,
            receiver,
            ingress: Arc::downgrade(ingress),
            runtime,
            registry: TaskRegistry::new(Arc::clone(&control)),
            scopes,
            mounted: Vec::new(),
            mounts: Vec::new(),
            has_mount_recipes: false,
            handles: HandleRegistryCore::new(),
            control,
            control_epoch: 0,
            commit_version: 0,
            task_groups: HashMap::new(),
            sequence_waits: HashMap::new(),
            sequences: HashMap::new(),
            next_sequence_id: 0,
            timers: TimerQueue::new(),
            task_timers: HashMap::new(),
            shutdown_reply: None,
            observations: ObservationRegistryCore::new(),
            #[cfg(not(feature = "diagnostics"))]
            guard: ActorGuard::new(lifecycle),
            #[cfg(feature = "diagnostics")]
            guard: Some(ActorGuard::new(lifecycle)),
            #[cfg(feature = "test-util")]
            assertion_gate,
            #[cfg(feature = "test-util")]
            transaction_gate: None,
            #[cfg(all(test, feature = "test-util"))]
            before_cancelled_gate_skip: None,
            #[cfg(all(test, feature = "test-util"))]
            before_gate_listen: None,
            #[cfg(test)]
            before_untimed_listener: None,
            #[cfg(test)]
            before_untimed_recheck: None,
        }
    }

    /// Binds declarations while the builder owns ingress and marks installation pending.
    pub(super) fn attach_mounts(&mut self, mounts: Vec<Box<C::Mount>>) {
        self.has_mount_recipes = mounts.iter().any(|mount| mount.can_discover());
        if !mounts.is_empty() {
            self.guard().lifecycle.pending_installation();
            let ingress = self
                .ingress
                .upgrade()
                .expect("builder owns ingress during binding");
            for mount in &mounts {
                mount.bind(&ingress, &self.guard().lifecycle);
            }
        }
        self.mounts = mounts;
    }

    /// Installs the optional observer before the actor starts processing commands.
    #[cfg(feature = "diagnostics")]
    pub(super) fn set_lifecycle_sink(&mut self, sink: Option<Box<C::LifecycleSink>>) {
        self.lifecycle_sink = sink;
    }

    /// Borrows the terminal guard, which remains present throughout the run loop.
    fn guard(&self) -> &ActorGuard {
        #[cfg(feature = "diagnostics")]
        {
            self.guard
                .as_ref()
                .expect("running actor retains its guard")
        }
        #[cfg(not(feature = "diagnostics"))]
        {
            &self.guard
        }
    }

    /// Borrows the guard before finalisation takes ownership of it.
    fn guard_mut(&mut self) -> &mut ActorGuard {
        #[cfg(feature = "diagnostics")]
        {
            self.guard
                .as_mut()
                .expect("running actor retains its guard")
        }
        #[cfg(not(feature = "diagnostics"))]
        {
            &mut self.guard
        }
    }

    /// Runs the serial loop and retains issue reporting until actor cleanup finishes.
    pub(crate) fn run(mut self) -> impl Future<Output = ()> + 'static {
        let close = crate::issue::IssueCloseGuard(self.issues.clone());
        let actor = async move {
            self.run_loop().await;
            self.finish_normal();
        };
        crate::issue::IssueLifetime { actor, close }
    }

    /// Arbitrates control, FIFO commands and due timers with bounded ready batches.
    ///
    /// Listeners are registered before rechecking state. Borrowed wait futures end
    /// before a command or timer action can mutate the actor again.
    #[expect(
        clippy::too_many_lines,
        reason = "timer arbitration keeps the borrowed wait boundary separate from owned processing"
    )]
    async fn run_loop(&mut self) {
        #[cfg(feature = "diagnostics")]
        let mut retained_head = if self.mounts.is_empty()
            && let Some(sink) = self.lifecycle_sink.as_deref_mut()
        {
            if self.receiver.is_closed() || !self.scopes.installation_is_live() {
                return;
            }
            let head = self.receiver.try_recv().ok();
            if !matches!(head, Some(Command::Shutdown(_))) {
                crate::diagnostics::LifecycleCall::new(
                    C::lifecycle_sink(sink),
                    self.commit_version,
                )
                .emit(crate::diagnostics::LifecycleKind::Installed {
                    lifetime: self.scopes.root().diagnostic_id(),
                    parent: None,
                    configuration: Some(self.scopes.configuration(self.scopes.root())),
                });
            }
            head
        } else {
            None
        };
        if !self.mounts.is_empty() && !self.install().await {
            return;
        }
        let mut prefer = ReadySource::Commands;
        let mut active_timer: Option<ActiveTimerWake<C::Future>> = None;
        let mut timer_free_turns = 0;
        loop {
            let control_turn = self.process_control();
            if !self.launch(control_turn) {
                break;
            }
            if self.timers.is_empty() {
                drop(active_timer.take());
                // An implicit-root prelude retains at most one admitted
                // head, consumed only after the ordinary control pass.
                #[cfg(feature = "diagnostics")]
                let outcome = if let Some(command) = retained_head.take() {
                    self.process_untimed_command(command).await
                } else {
                    self.wait_without_timers(&mut timer_free_turns).await
                };
                #[cfg(not(feature = "diagnostics"))]
                let outcome = self.wait_without_timers(&mut timer_free_turns).await;
                match outcome {
                    WaitOutcome::Continue => {
                        prefer = ReadySource::Commands;
                        // Control-only wakes also consume the cooperative budget.
                        timer_free_turns += 1;
                        if timer_free_turns == COMMAND_BATCH_LIMIT {
                            timer_free_turns = 0;
                            C::yield_now(&self.runtime).await;
                        }
                    }
                    WaitOutcome::Shutdown => {
                        return;
                    }
                    WaitOutcome::Closed => return,
                }
                continue;
            }
            timer_free_turns = 0;

            if prefer == ReadySource::Timers && self.timers.has_due(C::now(&self.runtime)) {
                if !self.process_due_timer_batch().await {
                    return;
                }
                prefer = ReadySource::Commands;
                continue;
            }

            prefer = ReadySource::Commands;
            let (processed, stopped) = self.process_command_batch().await;
            if stopped {
                drop(active_timer.take());
                return;
            }
            if processed != 0 {
                prefer = ReadySource::Timers;
                if processed == COMMAND_BATCH_LIMIT {
                    C::yield_now(&self.runtime).await;
                }
                continue;
            }

            if self.timers.has_due(C::now(&self.runtime)) {
                if !self.process_due_timer_batch().await {
                    return;
                }
                prefer = ReadySource::Commands;
                continue;
            }

            let wake = {
                // Register before checking the epoch; keep the pinned listener
                // and all losing borrowed futures inside this wait boundary.
                event_listener::listener!(self.control.event() => listener);
                if self.control.epoch() != self.control_epoch {
                    continue;
                }

                let next_wake = self.timers.next_wake();
                if active_timer.as_ref().map(ActiveTimerWake::key) != next_wake {
                    active_timer = next_wake.map(|(deadline, generation)| ActiveTimerWake {
                        deadline,
                        generation,
                        sleep: C::sleep_until(&self.runtime, deadline),
                    });
                }

                // The epoch may already include an admission whose command
                // remains queued. Inspect the queue after registration too.
                match self.receiver.try_recv() {
                    Ok(command) => TimedWake::Command(Ok(command)),
                    Err(async_channel::TryRecvError::Closed) => {
                        TimedWake::Command(Err(async_channel::RecvError))
                    }
                    Err(async_channel::TryRecvError::Empty) => {
                        let control = listener.fuse();
                        let timer = match active_timer.as_mut() {
                            Some(wake) => Either::Left(std::pin::Pin::new(&mut wake.sleep)),
                            None => Either::Right(std::future::pending()),
                        }
                        .fuse();
                        futures_util::pin_mut!(control, timer);
                        match select(control, timer).await {
                            Either::Left(((), _)) => TimedWake::Control,
                            Either::Right(((), _)) => TimedWake::Timer,
                        }
                    }
                }
            };
            match wake {
                TimedWake::Command(Ok(command)) => {
                    let turn = self.process_command(command).await;
                    if !self.launch(turn) {
                        drop(active_timer.take());
                        return;
                    }
                    prefer = ReadySource::Timers;
                }
                TimedWake::Command(Err(_closed)) => return,
                TimedWake::Control => {}
                TimedWake::Timer => {
                    // Keep the completed sleep alive until timer actions finish processing.
                    let _wake = active_timer.take();
                    if !self.process_timer_batch().await {
                        return;
                    }
                    prefer = ReadySource::Commands;
                }
            }
        }
    }

    /// Installs structural state without overtaking an already-admitted shutdown.
    ///
    /// At most one ordinary head is retained across installation; later commands
    /// stay in the mailbox and cannot be scanned for terminal priority.
    async fn install(&mut self) -> bool {
        if self.receiver.is_closed() {
            return false;
        }
        // Exactly one admitted head may precede installation. An ordinary head
        // is retained here, never scanned past or requeued for terminal priority.
        let head = self.receiver.try_recv().ok();
        if let Some(Command::Shutdown(reply)) = head {
            let turn = self.process_released_command(Command::Shutdown(reply));
            return self.launch(turn);
        }
        #[cfg(feature = "test-util")]
        let mut head = head;
        #[cfg(feature = "test-util")]
        let turn = if let Some(binding) = &self.transaction_gate {
            let gate = binding.gate().clone();
            gate.begin(
                crate::testing::PendingTransaction::Structural,
                self.runtime_diagnostics(),
                false,
            );
            self.release_typed_transaction(gate, Some(&mut head)).await
        } else {
            self.process_released_command(Command::Structural)
        };
        #[cfg(not(feature = "test-util"))]
        let turn = self.process_released_command(Command::Structural);
        if !self.launch(turn) {
            return false;
        }
        if let Some(command) = head {
            let turn = self.process_command(command).await;
            return self.launch(turn);
        }
        true
    }

    /// Runs a bounded ready-command batch, returning its count and whether it stopped.
    async fn process_command_batch(&mut self) -> (usize, bool) {
        let mut processed = 0;
        while processed < COMMAND_BATCH_LIMIT {
            let Ok(command) = self.receiver.try_recv() else {
                break;
            };
            let turn = self.process_command(command).await;
            if !self.launch(turn) {
                return (processed, true);
            }
            processed += 1;
        }
        (processed, false)
    }

    /// Processes one due batch, then yields before another source gets its turn.
    async fn process_due_timer_batch(&mut self) -> bool {
        if !self.process_timer_batch().await {
            return false;
        }
        C::yield_now(&self.runtime).await;
        true
    }

    /// Waits for admission or control, resetting the budget only on an actual pending poll.
    async fn wait_without_timers(&mut self, timer_free_turns: &mut usize) -> WaitOutcome {
        if self.control.epoch() != self.control_epoch {
            return WaitOutcome::Continue;
        }

        let command = match self.receiver.try_recv() {
            Ok(command) => Some(Ok(command)),
            Err(async_channel::TryRecvError::Closed) => Some(Err(async_channel::RecvError)),
            Err(async_channel::TryRecvError::Empty) => {
                // Ready commands need no listener. Before waiting, register and recheck both sources.
                #[cfg(test)]
                if let Some(before_listener) = self.before_untimed_listener.take() {
                    before_listener();
                }
                event_listener::listener!(self.control.event() => listener);
                if self.control.epoch() != self.control_epoch {
                    return WaitOutcome::Continue;
                }

                #[cfg(test)]
                if let Some(before_recheck) = self.before_untimed_recheck.take() {
                    before_recheck();
                }
                match self.receiver.try_recv() {
                    Ok(command) => Some(Ok(command)),
                    Err(async_channel::TryRecvError::Closed) => Some(Err(async_channel::RecvError)),
                    Err(async_channel::TryRecvError::Empty) => {
                        let mut listener = std::pin::pin!(listener);
                        std::future::poll_fn(|context| {
                            let result = listener.as_mut().poll(context);
                            // Empty alone is insufficient: a racing notification can be ready.
                            if result.is_pending() {
                                *timer_free_turns = 0;
                            }
                            result
                        })
                        .await;
                        None
                    }
                }
            }
        };
        match command {
            Some(Ok(command)) => self.process_untimed_command(command).await,
            Some(Err(_closed)) => WaitOutcome::Closed,
            None => WaitOutcome::Continue,
        }
    }

    /// Lets test frontiers suspend a timer-free command before launching its work.
    #[cfg(feature = "test-util")]
    async fn process_untimed_command(&mut self, command: Command<State, Action, C>) -> WaitOutcome {
        let turn = self.process_command(command).await;
        self.launch_untimed(turn)
    }

    /// Processes a timer-free command without an additional suspended async frame.
    #[cfg(not(feature = "test-util"))]
    fn process_untimed_command(
        &mut self,
        command: Command<State, Action, C>,
    ) -> std::future::Ready<WaitOutcome> {
        let turn = self.process_released_command(command);
        std::future::ready(self.launch_untimed(turn))
    }

    /// Distinguishes a graceful terminal command from an otherwise closed actor.
    fn launch_untimed(&mut self, turn: Turn<C::Future>) -> WaitOutcome {
        if self.launch(turn) {
            WaitOutcome::Continue
        } else if self.shutdown_reply.is_some() {
            WaitOutcome::Shutdown
        } else {
            WaitOutcome::Closed
        }
    }

    /// Exposes inspectable live mutations to a test frontier before committing them.
    #[cfg(feature = "test-util")]
    async fn process_command(&mut self, command: Command<State, Action, C>) -> Turn<C::Future> {
        // A retired output declaration cannot publish its typed inspection
        // slot. Reject stale authority through the normal transaction path
        // before making an otherwise uninspectable frontier visible.
        if let Command::Output {
            provenance,
            authority,
            ..
        }
        | Command::Mounted {
            provenance,
            authority,
            ..
        } = &command
            && self
                .transaction_parts()
                .rejection(provenance, authority)
                .is_some()
        {
            return self.process_released_command(command);
        }
        if let Command::Mounted { call, .. } = &command
            && call.input_rejection(&self.scopes).is_some()
        {
            return self.process_released_command(command);
        }
        let command = if let Some(binding) = &self.transaction_gate {
            match crate::testing::PendingTransaction::from_command(command) {
                Ok(pending) => {
                    let root = pending.is_root();
                    let gate = binding.gate().clone();
                    gate.begin(pending, self.runtime_diagnostics(), root);
                    return self.release_typed_transaction(gate, None).await;
                }
                Err(command) => command,
            }
        } else {
            command
        };
        let Command::Action(action) = command else {
            return self.process_released_command(command);
        };
        let gate = match self.begin_gated_effect(action) {
            Either::Left(command) => {
                return self.process_released_command(Command::Action(command));
            }
            Either::Right(gate) => gate,
        };
        let command = match self.release_gated_effect(gate).await {
            GateRelease::Action(command) => command,
            GateRelease::Skipped => return Turn::empty(true),
            GateRelease::Closed => return Turn::empty(false),
        };
        self.process_released_command(Command::Action(command))
    }

    /// Keeps the common awaitable dispatch interface without storing a test frontier.
    #[cfg(not(feature = "test-util"))]
    fn process_command(
        &mut self,
        command: Command<State, Action, C>,
    ) -> std::future::Ready<Turn<C::Future>> {
        std::future::ready(self.process_released_command(command))
    }

    /// Executes one released command and retains operation work through its commit.
    ///
    /// Closed ingress rejects before touching state. Shutdown drains admitted
    /// followers but withholds its reply until finalisation releases actor resources.
    #[expect(
        clippy::too_many_lines,
        reason = "exhaustive dispatch keeps command guards, terminal preparation and operation-work drop ordering visible"
    )]
    fn process_released_command(&mut self, command: Command<State, Action, C>) -> Turn<C::Future> {
        if self.receiver.is_closed() {
            command.close();
            return Turn::rejected(crate::TaskAccessError::new(
                crate::TaskAccessReason::StoreClosed,
                crate::TaskProgress::NotStarted,
            ));
        }
        let state = self.state.as_ref().expect("running actor owns state");
        match command {
            Command::Structural => {
                #[cfg(feature = "test-util")]
                let terminal = super::task_context::TerminalGuard::new(
                    self.transaction_gate
                        .as_ref()
                        .and_then(|binding| binding.gate().terminal_receipt()),
                );
                let parts = self.transaction_parts();
                #[cfg(feature = "test-util")]
                let parts = {
                    let mut parts = parts;
                    parts.structural_terminal = Some(terminal);
                    parts
                };
                return super::transaction::commit(parts, Mutation::Structural);
            }
            Command::Acquire(call) => call.execute(&self.scopes, &self.guard().lifecycle),
            Command::Fold(call) => {
                call.execute(state, &self.scopes, &self.mounts, &self.guard().lifecycle);
            }
            Command::Action(command) => {
                let mut turn = self.commit_action(command);
                turn.flow = true;
                return turn;
            }
            Command::Task {
                request,
                provenance,
                authority,
                operation_work,
            } => {
                let mut turn = super::transaction::request(
                    self.transaction_parts(),
                    request,
                    provenance,
                    authority,
                );
                drop(operation_work);
                turn.flow = true;
                return turn;
            }
            Command::Mounted {
                call,
                provenance,
                authority,
                operation_work,
            } => {
                let mut turn = super::transaction::commit(
                    self.transaction_parts(),
                    super::transaction::Mutation::Mounted(call, provenance, authority),
                );
                drop(operation_work);
                turn.flow = true;
                return turn;
            }
            Command::Output {
                call,
                provenance,
                authority,
                operation_work,
            } => {
                let mut turn = super::transaction::commit(
                    self.transaction_parts(),
                    super::transaction::Mutation::Output(call, provenance, authority),
                );
                drop(operation_work);
                turn.flow = true;
                return turn;
            }
            Command::Operation { request, metadata } => {
                return super::transaction::operation(
                    self.transaction_parts(),
                    request,
                    &*metadata,
                );
            }
            Command::OwnedAction(owned) => {
                if owned.cancellation.is_cancelled() {
                    let _ = owned.command.reply.send(Err(CommitError::Cancelled));
                } else {
                    let mut turn =
                        self.commit_action_with_wait(owned.command, Some(&owned.cancellation));
                    turn.flow = true;
                    return turn;
                }
            }
            Command::Read(command) => command.execute(state),
            Command::ScopedRead { scope, command } => {
                self.handles.with_validation(state, &scope, |validation| {
                    command.execute(state, validation);
                });
            }
            Command::RegisterHandle(command) => self.handles.register(state, command),
            Command::Observation(command) => {
                C::observe(
                    command,
                    state,
                    &mut self.observations,
                    self.commit_version,
                    self.scopes.root(),
                    None,
                    &super::scope::SelectedChild::root(state),
                );
            }
            Command::ScopedObservation { scope, command } => {
                self.handles.with_validation(state, &scope, |validation| {
                    C::observe(
                        command,
                        state,
                        &mut self.observations,
                        self.commit_version,
                        self.scopes.root(),
                        Some(scope.downgrade()),
                        validation,
                    );
                });
            }
            #[cfg(feature = "test-util")]
            Command::Snapshot(reply) => {
                let _ = reply.try_send(self.runtime_diagnostics());
            }
            Command::Shutdown(reply) => {
                self.receiver.close();
                self.guard_mut().mark_graceful();
                while let Ok(pending) = self.receiver.try_recv() {
                    pending.close();
                }

                self.shutdown_reply = Some(reply);
                return Turn::empty(false);
            }
        }
        Turn::empty(true)
    }

    /// Binds the test frontier to this actor's root identity before processing begins.
    #[cfg(feature = "test-util")]
    pub(crate) fn set_transaction_gate(
        &mut self,
        gate: Option<crate::testing::TransactionGateBinding<State, Action, C>>,
    ) {
        if let Some(binding) = &gate {
            binding.gate().bind_root(self.scopes.root_identity());
        }
        self.transaction_gate = gate;
    }

    /// Closes a typed frontier and waits for its owned inspection work to retire.
    #[cfg(feature = "test-util")]
    async fn retire_typed_transaction(
        gate: &crate::testing::TransactionGateCore<State, Action, C>,
    ) {
        gate.close();
        loop {
            let changed = gate.listen();
            if gate.retirement_finished() {
                return;
            }
            changed.await;
        }
    }

    /// Services control and queries while a typed transaction awaits a test decision.
    ///
    /// A startup frontier may retain one FIFO head. Committed work is launched
    /// once, while its actual result remains inspectable until the frontier clears.
    #[cfg(feature = "test-util")]
    async fn release_typed_transaction(
        &mut self,
        gate: crate::testing::TransactionGateCore<State, Action, C>,
        mut startup_head: Option<&mut Option<Command<State, Action, C>>>,
    ) -> Turn<C::Future> {
        let mut committed = None;
        loop {
            let changed = gate.listen().fuse();
            let control = self.control.listen().fuse();
            if self.receiver.is_closed() {
                Self::retire_typed_transaction(&gate).await;
                return Turn::empty(false);
            }
            if committed.is_none()
                && let Some(head) = startup_head.as_deref_mut()
                && head.is_none()
                && let Ok(command) = self.receiver.try_recv()
            {
                if matches!(command, Command::Shutdown(_)) {
                    Self::retire_typed_transaction(&gate).await;
                    return self.process_released_command(command);
                }
                *head = Some(command);
            }
            let control_turn = self.process_control();
            if !self.launch(control_turn) {
                Self::retire_typed_transaction(&gate).await;
                return Turn::empty(false);
            }
            if committed.is_none() && !gate.pending_is_live() {
                gate.rejected(crate::TaskAccessError::new(
                    crate::TaskAccessReason::Cancelled,
                    crate::TaskProgress::NotStarted,
                ));
            }
            while let Some(query) = gate.take_query() {
                query.execute(
                    self.state.as_ref().expect("running actor owns state"),
                    &self.scopes,
                    &self.mounts,
                    &self.guard().lifecycle,
                );
            }
            if gate.boundary().is_none() {
                return committed.take().unwrap_or_else(|| Turn::empty(true));
            }
            if let Some((pending, decision)) = gate.take_decided() {
                if decision == crate::testing::GateDecision::Skip {
                    // Clearing the frontier and rejecting the owned command are separate:
                    // neither user capture is destroyed while the gate lock is held.
                    gate.clear();
                    pending.reject();
                    return Turn::empty(true);
                }
                let turn = self.process_released_command(pending.into_command());
                match turn
                    .receipt
                    .clone()
                    .expect("selected mutations return their actual result")
                {
                    Ok(receipt) => {
                        gate.committed(receipt, self.runtime_diagnostics());
                        committed = Some(Turn::empty(turn.flow));
                        self.launch(turn);
                    }
                    Err(error) => {
                        gate.rejected(error);
                        if !self.launch(turn) {
                            return Turn::empty(false);
                        }
                    }
                }
                continue;
            }
            futures_util::pin_mut!(changed, control);
            let _ = select(changed, control).await;
        }
    }

    /// Diverts only ready effect actions into the legacy assertion frontier.
    #[cfg(feature = "test-util")]
    fn begin_gated_effect(
        &mut self,
        action: ActionCommand<Action>,
    ) -> Either<ActionCommand<Action>, crate::testing::AssertionGate<Action>> {
        let action = match action {
            ActionCommand {
                input: ActionInput::Ready { action, provenance },
                reply,
                assertion_reply,
                operation_work,
            } if provenance.is_effect() => ReadyActionCommand {
                action,
                provenance,
                reply,
                assertion_reply,
                operation_work,
            },
            command => return Either::Left(command),
        };
        let Some(binding) = self.assertion_gate.as_ref() else {
            return Either::Left(action.into_command());
        };
        let gate = binding.gate().clone();

        gate.begin(action, self.runtime_diagnostics());
        Either::Right(gate)
    }

    /// Waits for a legacy frontier decision while cancellation can still reject its action.
    #[cfg(feature = "test-util")]
    async fn release_gated_effect(
        &mut self,
        gate: crate::testing::AssertionGate<Action>,
    ) -> GateRelease<Action> {
        loop {
            if self.receiver.is_closed() {
                if let Some(command) = gate.force_skip() {
                    let _ = command.reply.send(Err(CommitError::Cancelled));
                }
                return GateRelease::Closed;
            }
            if !gate.pending_is_live() {
                #[cfg(test)]
                if let Some(before_skip) = self.before_cancelled_gate_skip.take() {
                    before_skip.await;
                }
                if let Some(command) = gate.force_skip() {
                    let _ = command.reply.send(Err(CommitError::Cancelled));
                }
                return GateRelease::Skipped;
            }
            while let Some(assertion) = gate.take_state_assertion() {
                assertion.execute(self.state.as_ref().expect("running actor owns state"));
            }
            if let Some(outcome) = gate.take_decided() {
                return match outcome.decision {
                    crate::testing::GateDecision::Release => {
                        GateRelease::Action(outcome.command.into_command())
                    }
                    crate::testing::GateDecision::Skip => {
                        let _ = outcome.command.reply.send(Err(CommitError::Cancelled));
                        GateRelease::Skipped
                    }
                };
            }

            #[cfg(test)]
            if let Some(before_listen) = self.before_gate_listen.take() {
                before_listen();
            }
            let gate_changed = gate.listen().fuse();
            let control_changed = self.control.listen().fuse();
            futures_util::pin_mut!(gate_changed, control_changed);
            if gate.is_decided() || gate.has_state_assertions() || self.receiver.is_closed() {
                continue;
            }
            let _wake = select(gate_changed, control_changed).await;
        }
    }

    /// Copies actor-owned counters and identities for an exhaustive test checkpoint.
    #[cfg(feature = "test-util")]
    fn runtime_snapshot(&self) -> crate::testing::RuntimeSnapshot {
        crate::testing::RuntimeSnapshot {
            commit_version: self.commit_version,
            live_task_ids: self.registry.live_task_ids(),
            cancellation_count: self.registry.cancellation_count(),
            timer_deadlines: self.timers.deadlines(),
            observation_count: self.observations.len(),
        }
    }

    /// Adds cancellation and scoped-handle detail to the current test snapshot.
    #[cfg(feature = "test-util")]
    fn runtime_diagnostics(&self) -> crate::testing::ActorDiagnostics {
        crate::testing::ActorDiagnostics::new(
            self.runtime_snapshot(),
            self.registry.cancellation_identities(),
            self.handles.len(),
        )
    }

    /// Releases resources and diagnostic callbacks before publishing terminal success.
    ///
    /// The lifecycle guard outlives those callbacks, so a panic prevents a successful
    /// shutdown or state handoff and publishes actor failure instead.
    fn finish_normal(mut self) {
        #[cfg(feature = "diagnostics")]
        if let Some(sink) = self.lifecycle_sink.take() {
            // Rebind the sink after the guard so actor/sink/state/reply unwind
            // drops while the lifecycle guard is armed. This is not a Drop callback.
            let guard = self.guard.take().expect("normal finalisation owns guard");
            let mut sink = sink;
            let reply = self.shutdown_reply.take();
            let state = if matches!(&reply, Some(TerminalReply::State(_))) {
                self.state.take()
            } else {
                None
            };
            let version = self.commit_version;
            let issues = self.issues.clone();
            drop(self);
            issues.close();
            let mut call =
                crate::diagnostics::LifecycleCall::new(C::lifecycle_sink(&mut *sink), version);
            call.emit(crate::diagnostics::LifecycleKind::IngressClosed);
            call.emit(crate::diagnostics::LifecycleKind::CancellationRequested {
                cause: crate::diagnostics::CancellationCause::ActorStop,
                scope: None,
            });
            call.emit(crate::diagnostics::LifecycleKind::ActorStopped);
            drop(sink);
            drop(guard);
            match reply {
                Some(TerminalReply::Shutdown(reply)) => {
                    let _ = reply.try_send(Ok(()));
                }
                Some(TerminalReply::State(reply)) => {
                    reply(state.expect("extraction retains original State"));
                }
                None => {}
            }
            return;
        }
        if let Some(reply) = self.shutdown_reply.take() {
            self.finish_shutdown(reply);
        }
    }

    /// Completes shutdown without diagnostics, preserving owned-state handoff ordering.
    fn finish_shutdown(mut self, reply: TerminalReply<C::Terminal>) {
        let issues = self.issues.clone();
        match reply {
            TerminalReply::Shutdown(reply) => {
                drop(self);
                issues.close();
                let _ = reply.try_send(Ok(()));
            }
            TerminalReply::State(reply) => {
                let state = self.state.take().expect("running actor owns state");
                drop(self);
                issues.close();
                reply(state);
            }
        }
    }

    /// Harvests terminal work and cancellation before preparing sequence successors.
    ///
    /// Runtime-dropped work stops the actor and discards any successors prepared
    /// earlier in the same batch. Their futures have not yet been handed to the host.
    fn process_control(&mut self) -> Turn<C::Future> {
        self.control_epoch = self.control.epoch();
        if !self.observations.is_empty() {
            self.observations.remove_inactive();
        }
        self.handles.remove_inactive();
        let control = self.registry.process_control();
        #[cfg(feature = "diagnostics")]
        if control.cancellation_applied
            && let Some(sink) = self.lifecycle_sink.as_deref_mut()
        {
            crate::diagnostics::LifecycleCall::new(C::lifecycle_sink(sink), self.commit_version)
                .emit(crate::diagnostics::LifecycleKind::CancellationRequested {
                    cause: crate::diagnostics::CancellationCause::Control,
                    scope: None,
                });
        }
        let terminal = control.terminal;
        let retired_work = !terminal.is_empty();
        let mut launches = Launches::default();
        for task in terminal {
            if task.status == TaskStatus::RuntimeDropped {
                return Turn::empty(false);
            }
            if !self.task_timers.is_empty() || !self.task_groups.is_empty() {
                self.transaction_parts().complete_task(task, &mut launches);
            }
        }
        // Terminal processing can destroy timer/sequence payloads after the
        // registry's fused pass. An idle roster cannot release that work.
        if retired_work {
            self.registry.prune_operations();
        }
        self.refresh_assertion_snapshot();
        Turn {
            flow: true,
            installation: false,
            launches,
            #[cfg(feature = "test-util")]
            receipt: None,
        }
    }

    /// Borrows disjoint actor resources for one transaction and samples its clock once.
    fn transaction_parts(&mut self) -> super::transaction::TransactionParts<'_, State, Action, C> {
        super::transaction::TransactionParts {
            state: self.state.as_mut().expect("running actor owns state"),
            reducer: &mut *self.reducer,
            dependencies: &mut self.dependencies,
            issues: &self.issues,
            #[cfg(feature = "diagnostics")]
            lifecycle_sink: self.lifecycle_sink.as_deref_mut(),
            ingress: &self.ingress,
            registry: &mut self.registry,
            scopes: &mut self.scopes,
            mounted: &mut self.mounted,
            mounts: &mut self.mounts,
            has_mount_recipes: self.has_mount_recipes,
            handles: &mut self.handles,
            control: &self.control,
            commit_version: &mut self.commit_version,
            task_groups: &mut self.task_groups,
            sequence_waits: &mut self.sequence_waits,
            sequences: &mut self.sequences,
            next_sequence_id: &mut self.next_sequence_id,
            timers: &mut self.timers,
            task_timers: &mut self.task_timers,
            observations: &mut self.observations,
            now: C::now(&self.runtime),
            #[cfg(feature = "test-util")]
            assertion_gate: &self.assertion_gate,
            #[cfg(feature = "test-util")]
            structural_terminal: None,
        }
    }

    /// Commits an action with no independent wait-cancellation token.
    fn commit_action(&mut self, command: ActionCommand<Action>) -> Turn<C::Future> {
        self.commit_action_with_wait(command, None)
    }

    /// Carries an owned observation wait's cancellation check into the mutation boundary.
    fn commit_action_with_wait(
        &mut self,
        command: ActionCommand<Action>,
        cancellation: Option<&crate::observation::wait::WaitCancellation>,
    ) -> Turn<C::Future> {
        super::transaction::commit(
            self.transaction_parts(),
            Mutation::Action(command, cancellation),
        )
    }

    /// Publishes completed installation before handing prepared futures to the runtime.
    ///
    /// Runtime spawn must not poll inline. Unspawned futures remain owned by the
    /// iterator and are dropped in order if a spawn call panics.
    fn launch(&self, turn: Turn<C::Future>) -> bool {
        if turn.installation {
            if self.guard().lifecycle.is_closed() {
                return false;
            }
            self.guard().lifecycle.mark_ready();
        }
        for launch in turn.launches {
            C::spawn(&self.runtime, launch);
        }
        turn.flow
    }

    /// Processes a finite due batch, rechecking cancellation before interval rescheduling.
    ///
    /// Typed frontiers defer this batch's terminal notifications until later due
    /// actions have had their ordinary opportunity to retire an earlier task's scope.
    #[expect(
        clippy::too_many_lines,
        reason = "one due-timer pass keeps cancellation precedence, interval rescheduling and deferred terminal publication ordered"
    )]
    async fn process_timer_batch(&mut self) -> bool {
        let due = self
            .timers
            .pop_due(C::now(&self.runtime), TIMER_BATCH_LIMIT);
        // A typed gate may drain unrelated runtime control while this finite
        // batch is suspended. Its own timer completions become continuable only
        // after later due actions have had their normal chance to retire scopes.
        #[cfg(feature = "test-util")]
        let mut completed = crate::effect::registry::TerminalTasks::Inline(None);
        #[cfg(feature = "test-util")]
        let gated = self.transaction_gate.is_some();
        #[cfg(feature = "test-util")]
        let mut complete = |registry: &TaskRegistry, id, status| {
            if gated {
                completed.push(crate::effect::registry::TerminalTask { id, status });
            } else {
                registry.complete_actor(id, status);
            }
        };
        #[cfg(not(feature = "test-util"))]
        let complete = TaskRegistry::complete_actor;
        for timer in due {
            // An earlier timer action can remove a scope and cancel another
            // timer already popped into this batch.
            if !self.registry.task_is_running(timer.payload.task) {
                self.task_timers.remove(&timer.payload.task);
                continue;
            }
            debug_assert_eq!(self.task_timers.get(&timer.payload.task), Some(&timer.id));
            match timer.payload.kind {
                super::mounted::DeferredTimer::Root(TimerKind::Wait) => {
                    self.task_timers.remove(&timer.payload.task);
                    let status = if timer.payload.provenance.is_live() {
                        TaskStatus::Completed
                    } else {
                        TaskStatus::Cancelled
                    };
                    complete(&self.registry, timer.payload.task, status);
                }
                super::mounted::DeferredTimer::Root(TimerKind::OneShot(action)) => {
                    self.task_timers.remove(&timer.payload.task);
                    if timer.payload.provenance.is_live() {
                        let Some(committed) = self
                            .commit_timer_action(action, &timer.payload.provenance)
                            .await
                        else {
                            self.refresh_assertion_snapshot();
                            return false;
                        };
                        let committed = self.launch(committed);
                        complete(
                            &self.registry,
                            timer.payload.task,
                            if committed {
                                TaskStatus::Completed
                            } else {
                                TaskStatus::Cancelled
                            },
                        );
                    } else {
                        complete(&self.registry, timer.payload.task, TaskStatus::Cancelled);
                    }
                }
                super::mounted::DeferredTimer::Root(TimerKind::Interval {
                    period,
                    mut make_action,
                }) => {
                    if timer.payload.provenance.is_live() {
                        let Some(turn) = self
                            .commit_timer_action(make_action(), &timer.payload.provenance)
                            .await
                        else {
                            self.refresh_assertion_snapshot();
                            return false;
                        };
                        self.launch(turn);
                        if !timer.payload.provenance.is_live() {
                            self.task_timers.remove(&timer.payload.task);
                            if self.registry.task_is_running(timer.payload.task) {
                                complete(&self.registry, timer.payload.task, TaskStatus::Cancelled);
                            }
                            continue;
                        }
                        let deadline = timer.deadline.saturating_add(period);
                        let next = self.timers.insert(
                            deadline,
                            TimerPayload {
                                task: timer.payload.task,
                                provenance: timer.payload.provenance,
                                kind: super::mounted::DeferredTimer::Root(TimerKind::Interval {
                                    period,
                                    make_action,
                                }),
                                operation_work: timer.payload.operation_work,
                            },
                        );
                        self.task_timers.insert(timer.payload.task, next);
                    } else {
                        self.task_timers.remove(&timer.payload.task);
                        complete(&self.registry, timer.payload.task, TaskStatus::Cancelled);
                    }
                }
                super::mounted::DeferredTimer::Routed(mut routed) => {
                    let provenance = timer.payload.provenance.as_provenance();
                    if self
                        .transaction_parts()
                        .rejection(&provenance, routed.authority())
                        .is_some()
                    {
                        self.task_timers.remove(&timer.payload.task);
                        complete(&self.registry, timer.payload.task, TaskStatus::Cancelled);
                        continue;
                    }
                    let period = routed.period();
                    let command = routed.fire(provenance);
                    let turn = self.process_command(command).await;
                    if !self.launch(turn) {
                        self.refresh_assertion_snapshot();
                        return false;
                    }
                    if let Some(period) = period {
                        if !timer.payload.provenance.is_live()
                            || !self.scopes.accepts(routed.authority())
                        {
                            self.task_timers.remove(&timer.payload.task);
                            if self.registry.task_is_running(timer.payload.task) {
                                complete(&self.registry, timer.payload.task, TaskStatus::Cancelled);
                            }
                            continue;
                        }
                        let next = self.timers.insert(
                            timer.deadline.saturating_add(period),
                            TimerPayload {
                                task: timer.payload.task,
                                provenance: timer.payload.provenance,
                                kind: super::mounted::DeferredTimer::Routed(routed),
                                operation_work: timer.payload.operation_work,
                            },
                        );
                        self.task_timers.insert(timer.payload.task, next);
                    } else {
                        self.task_timers.remove(&timer.payload.task);
                        if self.registry.task_is_running(timer.payload.task) {
                            complete(&self.registry, timer.payload.task, TaskStatus::Completed);
                        }
                    }
                }
            }
        }
        #[cfg(feature = "test-util")]
        for task in completed {
            // A retirement processed during a gated action may already have
            // cancelled and harvested this timer. Cancellation keeps priority.
            if self.registry.task_is_running(task.id) {
                self.registry.complete_actor(task.id, task.status);
            }
        }
        self.refresh_assertion_snapshot();
        true
    }

    /// Refreshes both supported test frontiers after runtime bookkeeping changes.
    #[cfg(feature = "test-util")]
    fn refresh_assertion_snapshot(&self) {
        if let Some(gate) = &self.assertion_gate {
            gate.gate().update_snapshot(self.runtime_diagnostics());
        }
        if let Some(gate) = &self.transaction_gate {
            gate.gate().update_snapshot(self.runtime_diagnostics());
        }
    }

    /// Keeps bookkeeping call sites identical when no test snapshots are compiled.
    #[cfg(not(feature = "test-util"))]
    #[expect(
        clippy::unused_self,
        reason = "call sites stay feature-independent without test diagnostics"
    )]
    fn refresh_assertion_snapshot(&self) {}

    /// Commits an actor-owned timer action through the configured test frontier.
    #[cfg(feature = "test-util")]
    async fn commit_timer_action(
        &mut self,
        action: Action,
        provenance: &TimerProvenance,
    ) -> Option<Turn<C::Future>> {
        if let Some(binding) = &self.transaction_gate {
            let gate = binding.gate().clone();
            let pending = crate::testing::PendingTransaction::Action(ReadyActionCommand {
                action,
                provenance: super::command::Provenance::Effect {
                    scope: provenance.scope,
                    lineage: Arc::clone(&provenance.lineage),
                    task: Weak::clone(&provenance.task),
                    generations: provenance.generations.clone(),
                    operations: provenance.operations.clone(),
                },
                reply: super::command::CommitSender::disconnected(),
                assertion_reply: None,
                operation_work: provenance.operations.retain_work(),
            });
            gate.begin(pending, self.runtime_diagnostics(), false);
            let turn = self.release_typed_transaction(gate, None).await;
            return turn.flow.then_some(turn);
        }
        let command = Self::timer_action_command(action, provenance);
        let gate = match self.begin_gated_effect(command) {
            Either::Left(command) => return Some(self.commit_action(command)),
            Either::Right(gate) => gate,
        };
        let command = match self.release_gated_effect(gate).await {
            GateRelease::Action(command) => command,
            GateRelease::Skipped => return Some(Turn::empty(true)),
            GateRelease::Closed => return None,
        };
        Some(self.commit_action(command))
    }

    /// Commits a timer action directly when no test frontier can suspend it.
    #[cfg(not(feature = "test-util"))]
    fn commit_timer_action(
        &mut self,
        action: Action,
        provenance: &TimerProvenance,
    ) -> std::future::Ready<Option<Turn<C::Future>>> {
        let command = Self::timer_action_command(action, provenance);
        std::future::ready(Some(self.commit_action(command)))
    }

    /// Transfers timer provenance into a command without allocating an unused reply channel.
    fn timer_action_command(action: Action, provenance: &TimerProvenance) -> ActionCommand<Action> {
        let reply = crate::store::command::CommitSender::disconnected();
        ActionCommand::effect(
            action,
            reply,
            provenance.scope,
            Arc::clone(&provenance.lineage),
            Weak::clone(&provenance.task),
            provenance.generations.clone(),
            provenance.operations.clone(),
            provenance.operations.retain_work(),
        )
    }
}

impl<State, Action, C: Carriers<State, Action>> Drop for Actor<State, Action, C> {
    fn drop(&mut self) {
        /// Releases retained commands if a producer or reply waker interrupts teardown.
        struct DrainBacklog<'a, T> {
            /// Armed until the explicit drain finishes; borrows without retaining ingress.
            receiver: Option<&'a super::mailbox::Receiver<T>>,
        }

        impl<T> Drop for DrainBacklog<'_, T> {
            fn drop(&mut self) {
                if let Some(receiver) = self.receiver {
                    while let Ok(pending) = receiver.try_recv() {
                        // Failure guards preserve ActorFailed and transaction progress.
                        // Explicit close would incorrectly report ordinary StoreClosed.
                        drop(pending);
                    }
                }
            }
        }

        let mut drain = DrainBacklog {
            receiver: Some(&self.receiver),
        };
        self.receiver.close();
        while let Ok(pending) = self.receiver.try_recv() {
            if std::thread::panicking() {
                drop(pending);
            } else {
                pending.close();
            }
        }
        drain.receiver = None;

        #[cfg(feature = "test-util")]
        if let Some(gate) = &self.assertion_gate
            && let Some(command) = gate.gate().force_skip()
        {
            let _ = command.reply.send(Err(CommitError::Cancelled));
        }
        self.registry.invalidate_all();
        self.observations.close_all();
    }
}

/// Actor-local identity for suspended, ordered effect segments.
pub(super) type SequenceId = u64;

/// Ready source given the next opportunity after the other completes a batch.
#[derive(Clone, Copy, Eq, PartialEq)]
enum ReadySource {
    /// Check admitted commands before another due-timer batch.
    Commands,
    /// Check due timers before another ready-command batch.
    Timers,
}

/// Result of a timer-free command or notification wait.
enum WaitOutcome {
    /// A command or control notification consumed one cooperative turn.
    Continue,
    /// An explicit terminal command is awaiting successful actor teardown.
    Shutdown,
    /// Admission or execution closed without an explicit terminal reply.
    Closed,
}

/// Owned result of the timed wait, after all borrowed wait futures have ended.
enum TimedWake<State, Action, C: Carriers<State, Action>> {
    /// An admitted FIFO command, or transport closure.
    Command(Result<Command<State, Action, C>, async_channel::RecvError>),
    /// Cancellation, completion or admission needs another control pass.
    Control,
    /// The retained sleep completed; due payloads still need validation.
    Timer,
}

/// Ownership transferred out of a legacy assertion frontier.
#[cfg(feature = "test-util")]
enum GateRelease<Action> {
    /// The test released this action for ordinary commit validation.
    Action(ActionCommand<Action>),
    /// The action was discarded without stopping the actor.
    Skipped,
    /// The store closed while the frontier was held.
    Closed,
}

/// Timer-owned work, retained until execution, cancellation or actor teardown.
pub(super) struct TimerPayload<
    State,
    Action,
    C: super::carriers::Carriers<State, Action> = super::carriers::Shared,
> {
    /// Registry identity whose completion this deadline will publish.
    pub(super) task: TaskId,
    /// Authority and cancellation snapshot checked before firing and rescheduling.
    pub(super) provenance: TimerProvenance,
    /// Unexecuted root action or mounted callback; drops before named-work ownership.
    pub(super) kind: super::mounted::DeferredTimer<State, Action, C>,
    /// Keeps named work unfinished until the timer payload is destroyed.
    pub(super) operation_work: crate::effect::operation::OperationWork,
}

/// One host sleep reused while the timer queue's earliest wake identity is unchanged.
struct ActiveTimerWake<F> {
    /// Earliest deadline selected when the sleep was constructed.
    deadline: crate::MonotonicInstant,
    /// Queue generation distinguishing changed entries at the same deadline.
    generation: u64,
    /// Host-owned wait retained through processing when it completes.
    sleep: F,
}

impl<F> ActiveTimerWake<F> {
    /// Returns the queue identity used to decide whether the host sleep is reusable.
    const fn key(&self) -> (crate::MonotonicInstant, u64) {
        (self.deadline, self.generation)
    }
}

/// Root timer behaviour; the carrier selects whether an interval callback needs Send.
pub(super) enum TimerKind<Action, I = Box<dyn FnMut() -> Action + Send>> {
    /// Completes a delay without producing an action.
    Wait,
    /// Commits one owned action if its provenance is still live.
    OneShot(Action),
    /// Produces actions lazily and reschedules from the preceding deadline.
    Interval {
        /// Validated nonzero interval used for drift-free deadline advancement.
        period: std::time::Duration,
        /// Called only after this tick's provenance has passed validation.
        make_action: I,
    },
}

/// Cancellation and operation context retained by actor-owned timers.
#[derive(Clone)]
pub(super) struct TimerProvenance {
    /// Structural lifetime used when converting the timer into a command.
    pub(super) scope: ScopeToken,
    /// Shared cancellation lineage for the originating action and its descendants.
    pub(super) lineage: Arc<LineageState>,
    /// Non-owning task identity; a retired registry entry cannot be revived by a timer.
    pub(super) task: Weak<TaskCompletion>,
    /// Immutable cancellation generations captured at registration.
    pub(super) generations: Box<[GenerationToken]>,
    /// Named-operation ancestry inherited by each produced action.
    pub(super) operations: crate::effect::operation::OperationAncestry,
}

impl TimerProvenance {
    /// Copies context for a routed command while the timer retains its next tick.
    pub(super) fn as_provenance(&self) -> super::command::Provenance {
        super::command::Provenance::Effect {
            scope: self.scope,
            lineage: Arc::clone(&self.lineage),
            task: Weak::clone(&self.task),
            generations: self.generations.clone(),
            operations: self.operations.clone(),
        }
    }

    /// Rejects cancelled, retired or superseded work before invoking a timer callback.
    pub(super) fn is_live(&self) -> bool {
        !self.lineage.is_cancelled()
            && self.task.upgrade().is_some_and(|task| task.is_running())
            && self.generations.iter().all(GenerationToken::is_live)
    }
}

/// Owned completion barrier reused by the consecutive segments of one sequence.
///
/// Registration retains one owner until compilation ends. Tasks and nested
/// sequences retain one owner each. Only explicit completion can advance work.
/// No weak references or non-member owners are retained between actor turns.
pub(super) struct WorkGroup {
    /// Sequence to continue once this segment's work has terminated.
    pub(super) sequence: SequenceId,
    /// Whether any completed member cancelled, preventing successful continuation.
    ///
    /// All accesses occur in the actor. Atomic storage keeps the shared owner
    /// sendable without requiring a lock or making captured effects `Sync`.
    pub(super) cancelled: AtomicBool,
}

/// Deferred sequence with cancellation and completion ownership spanning all segments.
pub(super) struct Sequence<Q> {
    /// Uncompiled segments, dropped before causal and named-work guards.
    pub(super) remaining: Q,
    /// Lifetime checked before starting each remaining segment.
    pub(super) scope: ScopeToken,
    /// Cancellation lineage shared by every segment.
    pub(super) lineage: Arc<LineageState>,
    /// Immutable sequence generations, copied into mutable input only when a segment starts.
    pub(super) generations: Box<[GenerationToken]>,
    /// Prefix inherited from enclosing work rather than explicit sequence declarations.
    pub(super) inherited_generations: usize,
    /// Outer segment whose ownership includes this sequence.
    pub(super) parent_group: Option<Arc<WorkGroup>>,
    /// Keeps the causal receipt unfinished until all deferred captures are released.
    pub(super) _work: LineageWork,
    /// Named-operation ancestry passed to each segment as it starts.
    pub(super) operations: crate::effect::operation::OperationAncestry,
    /// Keeps named work unfinished through destruction of all preceding fields.
    pub(super) _operation_work: crate::effect::operation::OperationWork,
}

/// Publishes actor termination only after the resources it guards have been released.
struct ActorGuard {
    /// Shared terminal state observed by waiters and submission endpoints.
    lifecycle: Arc<Lifecycle>,
    /// Intended non-panicking outcome; unwinding always selects failure instead.
    reason: CloseState,
}

impl ActorGuard {
    /// Arms abortive termination until an explicit shutdown command is accepted.
    fn new(lifecycle: Arc<Lifecycle>) -> Self {
        Self {
            lifecycle,
            reason: CloseState::Abortive,
        }
    }

    /// Records graceful intent without acknowledging teardown yet.
    fn mark_graceful(&mut self) {
        self.reason = CloseState::Graceful;
    }
}

impl Drop for ActorGuard {
    fn drop(&mut self) {
        let state = if std::thread::panicking() {
            CloseState::Failed
        } else {
            self.reason
        };
        self.lifecycle.close(state);
    }
}

#[cfg(test)]
ordinary_test_items! {
    #[path = "mounted/actor_tests.rs"]
    mod mounted_tests;
    mod tests {
        use super::test_support::{LaunchDriver, launch_actor};
        use std::fmt;
        use std::future::pending;
        use std::sync::atomic::{AtomicUsize, Ordering};

        use super::*;
        use crate::observation::ObservationSignal;
        use crate::store::observation::new_registration;
        use crate::store::scope::{ScopeIdentity, ScopeSlot, ScopeTransitionBatch};
        use crate::{ActionTask, MonotonicInstant, Reducer, RuntimeDriver};

        /// Checks a control notification that precedes listener registration and cannot wake that listener.
        #[test]
        fn untimed_wait_rechecks_control_after_listener_registration() {
            let (mut actor, _ingress) = launch_actor(Effect::none(), Runtime::new(TestDriver));
            let control = Arc::clone(&actor.control);
            actor.before_untimed_listener = Some(Box::new(move || control.notify()));
            let mut turns = 17;

            assert!(matches!(
                actor.wait_without_timers(&mut turns).now_or_never(),
                Some(WaitOutcome::Continue)
            ));
            assert_eq!(turns, 17, "a ready control wake does not reset the budget");
            assert_eq!(actor.control.epoch(), 1);
            assert_eq!(
                actor.control_epoch, 0,
                "the outer control pass must harvest the wake"
            );
            assert_eq!(actor.commit_version, 0);
            assert!(actor.before_untimed_listener.is_none());
        }

        /// Checks admission after the final epoch read without requiring another actor poll.
        #[test]
        fn untimed_wait_rechecks_admission_after_the_final_epoch_read() {
            let (mut actor, ingress) = launch_actor(Effect::none(), Runtime::new(TestDriver));
            let (reply, receive) = crate::store::command::commit_channel();
            let admission = Arc::clone(&ingress);
            actor.before_untimed_recheck = Some(Box::new(move || {
                admission
                    .try_send(Command::Action(ActionCommand::external((), reply)))
                    .unwrap();
            }));
            let mut turns = 17;

            assert!(matches!(
                actor.wait_without_timers(&mut turns).now_or_never(),
                Some(WaitOutcome::Continue)
            ));
            let task = receive
                .now_or_never()
                .expect("the same actor poll acknowledges the action")
                .expect("the actor retains the commit sender")
                .expect("the action commits");
            assert!(task.finish().now_or_never().is_some());
            assert!(!task.is_cancelled());
            assert_eq!(turns, 17, "a ready command does not reset the budget");
            assert_eq!(actor.commit_version, 1);
            assert!(actor.before_untimed_recheck.is_none());
        }

        /// Checks final-owner loss after the final epoch read without parking on an obsolete listener.
        #[test]
        fn untimed_wait_rechecks_closure_after_the_final_epoch_read() {
            let (mut actor, ingress) = launch_actor(Effect::none(), Runtime::new(TestDriver));
            actor.before_untimed_recheck = Some(Box::new(move || drop(ingress)));
            let mut turns = 17;

            assert!(matches!(
                actor.wait_without_timers(&mut turns).now_or_never(),
                Some(WaitOutcome::Closed)
            ));
            assert_eq!(turns, 17, "a closed mailbox does not reset the budget");
            assert!(actor.receiver.is_closed());
            assert_eq!(actor.commit_version, 0);
            assert!(actor.before_untimed_recheck.is_none());
        }

        #[cfg(feature = "test-util")]
        type GateAction = (u8, Arc<()>);

        #[cfg(feature = "test-util")]
        #[expect(
            clippy::needless_pass_by_value,
            reason = "the helper owns the assertion gate until actor construction finishes"
        )]
        /// Builds an actor whose emitted action can be held across cancellation and inspection.
        fn gate_boundary_actor(
            gate: crate::testing::AssertionGate<GateAction>,
            reductions: Arc<AtomicUsize>,
            tasks: Arc<std::sync::Mutex<Vec<RuntimeFuture>>>,
        ) -> (Actor<u8, GateAction>, crate::Store<u8, GateAction>) {
            let (sender, receiver) = crate::store::mailbox::bounded(
                std::num::NonZeroUsize::new(8).expect("test capacity is nonzero"),
            );
            let ingress = Arc::new(Ingress::new(sender, ControlWake::new()));
            let lifecycle = Arc::new(Lifecycle::new());
            let reducer = crate::Reduce::new(move |state: &mut u8, action: &GateAction, _| {
                reductions.fetch_add(1, Ordering::SeqCst);
                *state += action.0;
                if action.0 == 1 {
                    Effect::send((2, Arc::clone(&action.1)))
                } else {
                    Effect::none()
                }
            });
            let actor = Actor::new(
                0,
                Box::new(reducer),
                DependencyValues::new(),
                crate::IssueReporter::default(),
                receiver,
                &ingress,
                Arc::clone(&lifecycle),
                Runtime::new(LaunchDriver {
                    tasks,
                    panic_after: usize::MAX,
                }),
                Some(gate.attach().expect("test actor owns gate")),
            );
            (actor, crate::Store::from_parts(ingress, lifecycle))
        }

        #[cfg(feature = "test-util")]
        #[test]
        fn cancelled_gate_without_pause_releases_pending_command() {
            let gate = crate::testing::AssertionGate::new();
            let reductions = Arc::new(AtomicUsize::new(0));
            let tasks = Arc::new(std::sync::Mutex::new(Vec::new()));
            let (actor, store) =
                gate_boundary_actor(gate.clone(), Arc::clone(&reductions), Arc::clone(&tasks));
            let payload = Arc::new(());
            let receipt = store.try_send((1, Arc::clone(&payload))).unwrap();
            let mut actor = Box::pin(actor.run());
            let mut context = std::task::Context::from_waker(std::task::Waker::noop());
            assert!(actor.as_mut().poll(&mut context).is_pending());
            let task = receipt.finish().now_or_never().unwrap().unwrap();
            let mut effect = {
                let mut tasks = tasks.lock().unwrap();
                assert_eq!(tasks.len(), 1);
                tasks.pop().unwrap()
            };
            assert!(effect.as_mut().poll(&mut context).is_pending());
            assert!(actor.as_mut().poll(&mut context).is_pending());
            assert_eq!(gate.inspect_pending(|action| action.0), Some(2));
            assert_eq!(Arc::strong_count(&payload), 2);

            task.cancel();
            assert!(actor.as_mut().poll(&mut context).is_pending());
            assert!(!gate.has_pending());
            assert!(gate.pending_snapshot().is_none());
            assert!(!gate.release_pending());
            assert_eq!(Arc::strong_count(&payload), 1);
            assert!(effect.as_mut().poll(&mut context).is_ready());
            assert!(actor.as_mut().poll(&mut context).is_pending());
            assert!(task.is_cancelled());
            assert!(task.finish().now_or_never().is_some());

            let mut read = Box::pin(store.read(|state| *state));
            assert!(read.as_mut().poll(&mut context).is_pending());
            assert!(actor.as_mut().poll(&mut context).is_pending());
            assert!(matches!(
                read.as_mut().poll(&mut context),
                std::task::Poll::Ready(Ok(1))
            ));
            drop(read);
            assert_eq!(reductions.load(Ordering::SeqCst), 1);
            assert!(tasks.lock().unwrap().is_empty());
            let closed = store.closed();
            drop(store);
            assert!(actor.as_mut().poll(&mut context).is_ready());
            assert!(closed.now_or_never().is_some());
        }

        #[cfg(feature = "test-util")]
        #[test]
        fn cancelled_gate_cleans_up_while_pending_command_is_inspected() {
            let gate = crate::testing::AssertionGate::new();
            let reductions = Arc::new(AtomicUsize::new(0));
            let tasks = Arc::new(std::sync::Mutex::new(Vec::new()));
            let (mut actor, store) =
                gate_boundary_actor(gate.clone(), Arc::clone(&reductions), Arc::clone(&tasks));
            let mut paused = false;
            actor.before_cancelled_gate_skip =
                Some(Box::pin(futures_util::future::poll_fn(move |_| {
                    if std::mem::replace(&mut paused, true) {
                        std::task::Poll::Ready(())
                    } else {
                        // Deliberately resumed by one explicit poll inside inspect_pending.
                        std::task::Poll::Pending
                    }
                })));
            let payload = Arc::new(());
            let receipt = store.try_send((1, Arc::clone(&payload))).unwrap();
            let mut actor = Box::pin(actor.run());
            let mut context = std::task::Context::from_waker(std::task::Waker::noop());
            assert!(actor.as_mut().poll(&mut context).is_pending());
            let task = receipt.finish().now_or_never().unwrap().unwrap();
            let mut effect = {
                let mut tasks = tasks.lock().unwrap();
                assert_eq!(tasks.len(), 1);
                tasks.pop().unwrap()
            };
            assert!(effect.as_mut().poll(&mut context).is_pending());
            assert!(actor.as_mut().poll(&mut context).is_pending());
            assert_eq!(gate.inspect_pending(|action| action.0), Some(2));
            assert_eq!(Arc::strong_count(&payload), 2);
            task.cancel();
            assert!(actor.as_mut().poll(&mut context).is_pending());
            // The cancellation predicate has run, but force_skip has not.
            assert!(gate.pending_snapshot().is_some());
            gate.inspect_pending(|action| {
                assert_eq!(action.0, 2);
                assert!(actor.as_mut().poll(&mut context).is_pending());
                assert!(!gate.has_pending());
                assert!(gate.pending_snapshot().is_none());
                assert!(!gate.release_pending());
                assert_eq!(Arc::strong_count(&payload), 2);
                assert_eq!(reductions.load(Ordering::SeqCst), 1);
            })
            .expect("inspection owns the actual pending effect command");
            assert_eq!(Arc::strong_count(&payload), 1);
            assert!(effect.as_mut().poll(&mut context).is_ready());
            assert!(actor.as_mut().poll(&mut context).is_pending());
            assert!(task.is_cancelled());
            assert!(task.finish().now_or_never().is_some());
            let mut read = Box::pin(store.read(|state| *state));
            assert!(read.as_mut().poll(&mut context).is_pending());
            assert!(actor.as_mut().poll(&mut context).is_pending());
            assert!(matches!(
                read.as_mut().poll(&mut context),
                std::task::Poll::Ready(Ok(1))
            ));
            drop(read);
            assert_eq!(reductions.load(Ordering::SeqCst), 1);
            assert!(tasks.lock().unwrap().is_empty());
            let closed = store.closed();
            drop(store);
            assert!(actor.as_mut().poll(&mut context).is_ready());
            assert!(closed.now_or_never().is_some());
        }

        #[cfg(feature = "test-util")]
        #[test]
        fn assertion_queued_before_gate_listener_runs_in_the_same_actor_poll() {
            let gate = crate::testing::AssertionGate::new();
            let reductions = Arc::new(AtomicUsize::new(0));
            let tasks = Arc::new(std::sync::Mutex::new(Vec::new()));
            let (mut actor, store) =
                gate_boundary_actor(gate.clone(), Arc::clone(&reductions), Arc::clone(&tasks));
            let assertions = Arc::new(AtomicUsize::new(0));
            let assertion_receipt = Arc::new(std::sync::Mutex::new(None));
            actor.before_gate_listen = Some(Box::new({
                let gate = gate.clone();
                let assertions = Arc::clone(&assertions);
                let assertion_receipt = Arc::clone(&assertion_receipt);
                move || {
                    *assertion_receipt.lock().unwrap() =
                        Some(gate.assert_state(move |state: &u8| {
                            assert_eq!(*state, 1);
                            assertions.fetch_add(1, Ordering::SeqCst);
                        }));
                }
            }));
            let payload = Arc::new(());
            let receipt = store.try_send((1, Arc::clone(&payload))).unwrap();
            let mut actor = Box::pin(actor.run());
            let mut context = std::task::Context::from_waker(std::task::Waker::noop());
            assert!(actor.as_mut().poll(&mut context).is_pending());
            let task = receipt.finish().now_or_never().unwrap().unwrap();
            let mut effect = {
                let mut tasks = tasks.lock().unwrap();
                assert_eq!(tasks.len(), 1);
                tasks.pop().unwrap()
            };
            assert!(effect.as_mut().poll(&mut context).is_pending());
            assert!(actor.as_mut().poll(&mut context).is_pending());
            assertion_receipt
                .lock()
                .unwrap()
                .take()
                .unwrap()
                .finish()
                .now_or_never()
                .expect("the second check executes the queued assertion without another actor poll")
                .unwrap();
            assert_eq!(assertions.load(Ordering::SeqCst), 1);
            assert_eq!(gate.inspect_pending(|action| action.0), Some(2));
            assert_eq!(reductions.load(Ordering::SeqCst), 1);
            assert_eq!(Arc::strong_count(&payload), 2);
            assert!(!task.is_cancelled());
            assert!(!task.is_finished());
            let released = gate.release_pending_with_receipt().unwrap();
            assert!(actor.as_mut().poll(&mut context).is_pending());
            let _released_task = released.finish().now_or_never().unwrap().unwrap();
            assert!(!gate.has_pending());
            assert_eq!(Arc::strong_count(&payload), 1);
            assert!(effect.as_mut().poll(&mut context).is_ready());
            assert!(actor.as_mut().poll(&mut context).is_pending());
            assert!(task.finish().now_or_never().is_some());
            assert!(!task.is_cancelled());
            assert_eq!(reductions.load(Ordering::SeqCst), 2);
            let mut read = Box::pin(store.read(|state| *state));
            assert!(read.as_mut().poll(&mut context).is_pending());
            assert!(actor.as_mut().poll(&mut context).is_pending());
            assert!(matches!(
                read.as_mut().poll(&mut context),
                std::task::Poll::Ready(Ok(3))
            ));
            drop(read);
            assert_eq!(assertions.load(Ordering::SeqCst), 1);
            assert!(tasks.lock().unwrap().is_empty());
            let closed = store.closed();
            drop(store);
            assert!(actor.as_mut().poll(&mut context).is_ready());
            assert!(closed.now_or_never().is_some());
        }


        /// Records capture destruction order independently of whether its factory was called.
        struct Captured(usize, Arc<std::sync::Mutex<Vec<usize>>>);

        impl Drop for Captured {
            fn drop(&mut self) {
                self.1.lock().unwrap().push(self.0);
            }
        }

        /// Separates factory invocation from capture destruction for panic-path assertions.
        fn tracked_effect(
            id: usize,
            drops: &Arc<std::sync::Mutex<Vec<usize>>>,
            calls: &Arc<AtomicUsize>,
        ) -> Effect<()> {
            let captured = Captured(id, Arc::clone(drops));
            let calls = Arc::clone(calls);
            Effect::run(move |_| {
                calls.fetch_add(1, Ordering::Relaxed);
                drop(captured);
                std::future::ready(())
            })
        }


        /// Counts transaction clock samples while reusing the launch fixture's ownership rules.
        ///
        /// Explicit `now` calls publish successive seconds. Waits observe the last
        /// published sample without incrementing the actor's clock-call counter.
        #[derive(Debug)]
        struct CountingClockDriver {
            /// Owns accepted work and supplies the cooperative yield behaviour.
            launch: LaunchDriver,
            /// Counts explicit clock samples, not wait construction or polling.
            now_calls: Arc<AtomicUsize>,
            /// Supplies the scenario's baseline time before its sample offset.
            now_seconds: Arc<AtomicUsize>,
        }

        impl RuntimeDriver for CountingClockDriver {
            fn spawn(&self, task: RuntimeFuture) {
                self.launch.spawn(task);
            }

            fn now(&self) -> MonotonicInstant {
                let sample = self.now_calls.fetch_add(1, Ordering::Relaxed);
                MonotonicInstant::from_elapsed(std::time::Duration::from_secs(
                    (self.now_seconds.load(Ordering::Relaxed) + sample) as u64,
                ))
            }

            fn sleep_until(&self, deadline: MonotonicInstant) -> RuntimeFuture {
                let now_calls = Arc::clone(&self.now_calls);
                let now_seconds = Arc::clone(&self.now_seconds);
                Box::pin(std::future::poll_fn(move |_| {
                    // `now_calls` names the next sample, not the last published one.
                    let published = now_calls.load(Ordering::Relaxed).saturating_sub(1);
                    let now = MonotonicInstant::from_elapsed(std::time::Duration::from_secs(
                        (now_seconds.load(Ordering::Relaxed) + published) as u64,
                    ));
                    if deadline <= now {
                        std::task::Poll::Ready(())
                    } else {
                        std::task::Poll::Pending
                    }
                }))
            }

            fn yield_now(&self) -> RuntimeFuture {
                self.launch.yield_now()
            }
        }

        #[test]
        fn launch_driver_honours_frozen_clock() {
            use std::task::{Context, Waker};

            let driver = LaunchDriver {
                tasks: Arc::new(std::sync::Mutex::new(Vec::new())),
                panic_after: usize::MAX,
            };
            let context = &mut Context::from_waker(Waker::noop());
            let mut elapsed = driver.sleep_until(MonotonicInstant::ZERO);
            assert!(
                elapsed.as_mut().poll(context).is_ready(),
                "elapsed deadline must be ready"
            );

            for deadline in [
                MonotonicInstant::from_elapsed(std::time::Duration::from_nanos(1)),
                MonotonicInstant::MAX,
            ] {
                let mut future = driver.sleep_until(deadline);
                assert!(future.as_mut().poll(context).is_pending());
                assert_eq!(driver.now(), MonotonicInstant::ZERO);
            }
        }

        #[test]
        fn counting_clock_driver_elapsed_wait_uses_its_own_origin_without_sampling() {
            use std::task::{Context, Waker};

            let now_calls = Arc::new(AtomicUsize::new(0));
            let driver = CountingClockDriver {
                launch: LaunchDriver {
                    tasks: Arc::new(std::sync::Mutex::new(Vec::new())),
                    panic_after: usize::MAX,
                },
                now_calls: Arc::clone(&now_calls),
                now_seconds: Arc::new(AtomicUsize::new(40)),
            };
            let mut elapsed = driver.sleep_until(MonotonicInstant::from_elapsed(
                std::time::Duration::from_secs(40),
            ));
            let context = &mut Context::from_waker(Waker::noop());

            assert_eq!(now_calls.load(Ordering::Relaxed), 0);
            assert!(
                elapsed.as_mut().poll(context).is_ready(),
                "the driver's nonzero origin is already due"
            );
            assert_eq!(
                now_calls.load(Ordering::Relaxed),
                0,
                "wait polling is not an actor clock sample"
            );
        }

        #[test]
        fn counting_clock_driver_existing_wait_rechecks_published_time_without_sampling() {
            use std::task::{Context, Waker};
            use std::time::Duration;

            let now_calls = Arc::new(AtomicUsize::new(0));
            let driver = CountingClockDriver {
                launch: LaunchDriver {
                    tasks: Arc::new(std::sync::Mutex::new(Vec::new())),
                    panic_after: usize::MAX,
                },
                now_calls: Arc::clone(&now_calls),
                now_seconds: Arc::new(AtomicUsize::new(40)),
            };
            let context = &mut Context::from_waker(Waker::noop());
            let mut future = driver.sleep_until(MonotonicInstant::from_elapsed(
                Duration::from_secs(42),
            ));
            let mut maximum = driver.sleep_until(MonotonicInstant::MAX);

            assert!(future.as_mut().poll(context).is_pending());
            assert!(maximum.as_mut().poll(context).is_pending());
            assert_eq!(now_calls.load(Ordering::Relaxed), 0);
            for second in 40..=41 {
                assert_eq!(
                    driver.now(),
                    MonotonicInstant::from_elapsed(Duration::from_secs(second))
                );
                assert!(
                    future.as_mut().poll(context).is_pending(),
                    "a wait cannot use the next unpublished sample"
                );
            }
            assert_eq!(now_calls.load(Ordering::Relaxed), 2);
            assert_eq!(
                driver.now(),
                MonotonicInstant::from_elapsed(Duration::from_secs(42))
            );
            assert!(
                future.as_mut().poll(context).is_ready(),
                "an existing wait must see the advanced origin"
            );
            assert!(maximum.as_mut().poll(context).is_pending());
            assert_eq!(
                now_calls.load(Ordering::Relaxed),
                3,
                "wait polling must not add actor clock samples"
            );
        }

        #[test]
        fn terminal_control_without_timers_or_groups_does_not_sample_clock() {
            let tasks = Arc::new(std::sync::Mutex::new(Vec::new()));
            let now_calls = Arc::new(AtomicUsize::new(0));
            let (mut actor, _ingress) = launch_actor(
                Effect::none(),
                Runtime::new(CountingClockDriver {
                    launch: LaunchDriver {
                        tasks: Arc::clone(&tasks),
                        panic_after: usize::MAX,
                    },
                    now_calls: Arc::clone(&now_calls),
                    now_seconds: Arc::new(AtomicUsize::new(0)),
                }),
            );
            let scope = actor.scopes.root();
            let mut lineage = None;
            let mut launches = Launches::default();
            actor.transaction_parts().compile_effect(
                Effect::future(std::future::ready(None)),
                super::super::mounted::WorkOrigin {
                    scope,
                    lineage: &mut lineage,
                    generations: &mut Vec::new(),
                    operations: &crate::effect::operation::OperationAncestry::default(),
                    inherited_generations: 0,
                    group: None,
                },
                &mut launches,
            );
            let lineage = lineage.expect("registered future owns a lineage");
            let witness = Arc::downgrade(&lineage);
            let task = ActionTask::live(Arc::clone(&lineage));
            lineage.finish_registration();
            assert!(!task.is_finished());
            for launch in launches {
                actor.runtime.spawn(launch);
            }
            let launch = tasks.lock().unwrap().pop().expect("one registered future");
            assert!(tasks.lock().unwrap().is_empty());
            assert!(
                launch.now_or_never().is_some(),
                "registered immediate future completes in one poll"
            );
            assert!(
                !task.is_finished(),
                "registry work stays live until terminal harvest"
            );
            assert!(!task.is_cancelled());
            assert!(actor.task_timers.is_empty());
            assert!(actor.task_groups.is_empty());

            now_calls.store(0, Ordering::Relaxed);
            let turn = actor.process_control();
            assert!(actor.launch(turn));
            let samples = now_calls.load(Ordering::Relaxed);
            assert!(task.is_finished());
            assert!(lineage.is_finished());
            assert!(!task.is_cancelled());
            assert!(actor.timers.is_empty());
            assert!(actor.task_timers.is_empty());
            assert!(actor.task_groups.is_empty());
            assert!(actor.sequence_waits.is_empty());
            assert!(actor.sequences.is_empty());
            assert!(actor.registry.process_control().terminal.is_empty());
            drop(task);
            drop(lineage);
            assert!(
                witness.upgrade().is_none(),
                "harvest releases the registry lineage"
            );
            eprintln!("terminal without bookkeeping: now_calls={samples}");
            assert_eq!(
                samples, 0,
                "terminal harvest needs no transaction clock without timers or groups"
            );
        }

        #[test]
        fn terminal_control_grouped_successor_uses_completion_clock() {
            let tasks = Arc::new(std::sync::Mutex::new(Vec::new()));
            let now_calls = Arc::new(AtomicUsize::new(0));
            let now_seconds = Arc::new(AtomicUsize::new(0));
            let (mut actor, _ingress) = launch_actor(
                Effect::concatenate([
                    Effect::future(std::future::ready(None)),
                    Effect::send_after(std::time::Duration::from_secs(5), ()),
                ]),
                Runtime::new(CountingClockDriver {
                    launch: LaunchDriver {
                        tasks: Arc::clone(&tasks),
                        panic_after: usize::MAX,
                    },
                    now_calls: Arc::clone(&now_calls),
                    now_seconds: Arc::clone(&now_seconds),
                }),
            );
            let (reply, mut receive) = crate::store::command::commit_channel();
            let turn = actor.commit_action(ActionCommand::external((), reply));
            assert!(actor.launch(turn));
            let task = receive.try_recv().unwrap().unwrap().unwrap();
            let launch = tasks
                .lock()
                .unwrap()
                .pop()
                .expect("sequence launches its first future");
            assert!(tasks.lock().unwrap().is_empty());
            assert!(
                launch.now_or_never().is_some(),
                "registered immediate future completes in one poll"
            );
            assert!(
                !task.is_finished(),
                "sequence still owns the timed successor"
            );
            assert!(!actor.task_groups.is_empty());
            assert!(actor.task_timers.is_empty());

            now_seconds.store(40, Ordering::Relaxed);
            now_calls.store(0, Ordering::Relaxed);
            let turn = actor.process_control();
            assert!(actor.launch(turn));
            assert_eq!(now_calls.load(Ordering::Relaxed), 1);
            let deadline = actor
                .timers
                .next_wake()
                .expect("successor timer must be installed")
                .0;
            assert_eq!(
                deadline,
                MonotonicInstant::from_elapsed(std::time::Duration::from_secs(45))
            );
            assert_eq!(actor.task_timers.len(), 1);
            assert_eq!(actor.task_groups.len(), 1);
            assert!(!task.is_finished());
            assert!(!task.is_cancelled());
            eprintln!("grouped timed successor: now_calls=1 deadline={deadline:?}");

            task.cancel();
            now_calls.store(0, Ordering::Relaxed);
            let turn = actor.process_control();
            assert!(actor.launch(turn));
            assert_eq!(
                now_calls.load(Ordering::Relaxed),
                1,
                "timer bookkeeping still uses the normal path"
            );
            assert!(task.is_finished());
            assert!(task.is_cancelled());
            assert!(actor.timers.is_empty());
            assert!(actor.task_timers.is_empty());
            assert!(actor.task_groups.is_empty());
            assert!(actor.sequence_waits.is_empty());
            assert!(actor.sequences.is_empty());
            assert!(actor.registry.process_control().terminal.is_empty());
        }

        #[test]
        fn terminal_control_ungrouped_timer_keeps_completion_bookkeeping() {
            let tasks = Arc::new(std::sync::Mutex::new(Vec::new()));
            let now_calls = Arc::new(AtomicUsize::new(0));
            let (mut actor, _ingress) = launch_actor(
                Effect::send_after(std::time::Duration::from_secs(5), ()),
                Runtime::new(CountingClockDriver {
                    launch: LaunchDriver {
                        tasks: Arc::clone(&tasks),
                        panic_after: usize::MAX,
                    },
                    now_calls: Arc::clone(&now_calls),
                    now_seconds: Arc::new(AtomicUsize::new(0)),
                }),
            );
            let (reply, mut receive) = crate::store::command::commit_channel();
            let turn = actor.commit_action(ActionCommand::external((), reply));
            assert!(actor.launch(turn));
            let task = receive.try_recv().unwrap().unwrap().unwrap();
            assert!(tasks.lock().unwrap().is_empty());
            assert_eq!(actor.task_timers.len(), 1);
            assert!(actor.task_groups.is_empty());
            assert!(!task.is_finished());

            task.cancel();
            now_calls.store(0, Ordering::Relaxed);
            let turn = actor.process_control();
            assert!(actor.launch(turn));
            assert_eq!(now_calls.load(Ordering::Relaxed), 1);
            assert!(task.is_finished());
            assert!(task.is_cancelled());
            assert!(actor.timers.is_empty());
            assert!(actor.task_timers.is_empty());
            assert!(actor.task_groups.is_empty());
            assert!(actor.sequence_waits.is_empty());
            assert!(actor.sequences.is_empty());
            assert!(actor.registry.process_control().terminal.is_empty());
        }

        // Private actor-node fixture: proves the ready control scheduler, not a public timer workload.
        #[test]
        fn control_only_ready_chain_yields_before_exhaustion() {
            use std::task::{Context, Poll, Wake, Waker};

            const BACKLOG: usize = 129;
            struct CancelNext(ActionTask);
            impl Wake for CancelNext {
                fn wake(self: Arc<Self>) {
                    self.0.cancel();
                }
            }

            let launches = Arc::new(std::sync::Mutex::new(Vec::new()));
            let (mut actor, ingress) = launch_actor(
                Effect::none(),
                Runtime::new(LaunchDriver {
                    tasks: Arc::clone(&launches),
                    panic_after: usize::MAX,
                }),
            );
            let scope = actor.scopes.root();
            let mut tasks = Vec::with_capacity(BACKLOG);
            let mut lineages = Vec::with_capacity(BACKLOG);
            for _ in 0..BACKLOG {
                let lineage = LineageState::new(Arc::downgrade(&actor.control));
                let prepared = actor.registry.prepare(scope, Arc::clone(&lineage), &[]);
                actor.registry.register_actor(prepared);
                lineage.finish_registration();
                lineages.push(Arc::downgrade(&lineage));
                tasks.push(ActionTask::live(lineage));
            }
            let mut waits: Vec<_> = tasks.iter().map(|task| Box::pin(task.finish())).collect();
            for (wait, next) in waits.iter_mut().zip(tasks.iter().skip(1)) {
                let waker = Waker::from(Arc::new(CancelNext(next.clone())));
                assert!(
                    wait.as_mut()
                        .poll(&mut Context::from_waker(&waker))
                        .is_pending()
                );
            }
            let mut context = Context::from_waker(Waker::noop());
            assert!(
                waits
                    .last_mut()
                    .unwrap()
                    .as_mut()
                    .poll(&mut context)
                    .is_pending()
            );
            assert!(actor.timers.is_empty());
            assert!(matches!(
                actor.receiver.try_recv(),
                Err(async_channel::TryRecvError::Empty)
            ));

            // Harvest releases the current node only after the cancellation scan.
            // Its finish wake cancels the next node, producing a fresh control-only turn.
            tasks[0].cancel();
            let mut actor = Box::pin(actor.run());
            assert!(actor.as_mut().poll(&mut context).is_pending());
            let finished = tasks.iter().filter(|task| task.is_finished()).count();
            assert!(finished > 0 && finished < BACKLOG);
            assert!(
                waits
                    .last_mut()
                    .unwrap()
                    .as_mut()
                    .poll(&mut context)
                    .is_pending()
            );
            for _ in 0..BACKLOG {
                if tasks.iter().all(ActionTask::is_finished) {
                    break;
                }
                assert!(actor.as_mut().poll(&mut context).is_pending());
            }
            assert!(tasks.iter().all(ActionTask::is_finished));
            assert!(tasks.iter().all(ActionTask::is_cancelled));
            for mut wait in waits {
                assert_eq!(wait.as_mut().poll(&mut context), Poll::Ready(()));
            }
            assert!(launches.lock().unwrap().is_empty());
            drop(ingress);
            assert!(actor.as_mut().poll(&mut context).is_ready());
            drop(actor);
            drop(tasks);
            assert!(lineages.iter().all(|lineage| lineage.upgrade().is_none()));
        }

        #[test]
        fn actual_idle_control_turn_avoids_a_second_named_record_scan() {
            for named in [0, 2] {
                let tasks = Arc::new(std::sync::Mutex::new(Vec::new()));
                let ordinary = (0..16).map(|key| Effect::future(pending()).cancellable(key));
                let named_effects = (0..named).map(|_| {
                    Effect::future(pending()).named("work", crate::OperationPolicy::Coexist)
                });
                let (mut actor, _ingress) = launch_actor(
                    Effect::merge(ordinary.chain(named_effects)),
                    Runtime::new(LaunchDriver {
                        tasks: Arc::clone(&tasks),
                        panic_after: usize::MAX,
                    }),
                );
                let (reply, _receive) = crate::store::command::commit_channel();
                let turn = actor.commit_action(ActionCommand::external((), reply));
                assert!(actor.launch(turn));
                assert_eq!(tasks.lock().unwrap().len(), 16 + named);
                crate::effect::registry::OPERATION_RECORD_VISITS.with(|visits| visits.set(0));
                let turn = actor.process_control();
                assert!(actor.launch(turn));
                let visits =
                    crate::effect::registry::OPERATION_RECORD_VISITS.with(std::cell::Cell::get);
                let expected = if named == 0 { 0 } else { 18 };
                eprintln!("actual idle control: ordinary=16 named={named} record_visits={visits}");
                assert_eq!(
                    visits, expected,
                    "an idle turn requires only the fused pre-harvest pass"
                );
            }
        }


        #[test]
        fn compile_panic_drops_queued_launches_without_spawning_or_acknowledging() {
            let drops = Arc::new(std::sync::Mutex::new(Vec::new()));
            let calls = Arc::new(AtomicUsize::new(0));
            let tasks = Arc::new(std::sync::Mutex::new(Vec::new()));
            let effect = Effect::merge([
                tracked_effect(0, &drops, &calls),
                Effect::concatenate([Effect::run(|_| async {}), Effect::run(|_| async {})]),
            ]);
            let (mut actor, _ingress) = launch_actor(
                effect,
                Runtime::new(LaunchDriver {
                    tasks: Arc::clone(&tasks),
                    panic_after: usize::MAX,
                }),
            );
            actor.next_sequence_id = SequenceId::MAX;
            let (reply, mut receive) = crate::store::command::commit_channel();
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let turn = actor.commit_action(ActionCommand::external((), reply));
                actor.launch(turn);
            }))
            .expect_err("sequence overflow interrupts compilation");
            receive
                .try_recv()
                .expect_err("failed compilation cannot acknowledge success");
            assert!(tasks.lock().unwrap().is_empty());
            assert_eq!(calls.load(Ordering::Relaxed), 0);
            assert_eq!(*drops.lock().unwrap(), [0]);
        }

        #[test]
        fn intermediate_spawn_panic_preserves_prior_launch_and_drops_remainder() {
            let drops = Arc::new(std::sync::Mutex::new(Vec::new()));
            let calls = Arc::new(AtomicUsize::new(0));
            let tasks = Arc::new(std::sync::Mutex::new(Vec::new()));
            let effect = Effect::merge((0..5).map(|id| tracked_effect(id, &drops, &calls)));
            let (mut actor, _ingress) = launch_actor(
                effect,
                Runtime::new(LaunchDriver {
                    tasks: Arc::clone(&tasks),
                    panic_after: 1,
                }),
            );
            let (reply, mut receive) = crate::store::command::commit_channel();
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let turn = actor.commit_action(ActionCommand::external((), reply));
                actor.launch(turn);
            }))
            .expect_err("second host spawn panics");
            assert!(matches!(receive.try_recv(), Ok(Some(Ok(_)))));
            assert_eq!(
                actor.commit_version, 1,
                "spawn failure does not erase commit"
            );
            assert_eq!(tasks.lock().unwrap().len(), 1);
            assert_eq!(calls.load(Ordering::Relaxed), 0);
            assert_eq!(*drops.lock().unwrap(), [1, 2, 3, 4]);
            let task = tasks.lock().unwrap().pop().unwrap();
            futures_executor::block_on(task);
            assert_eq!(calls.load(Ordering::Relaxed), 1);
            assert_eq!(*drops.lock().unwrap(), [1, 2, 3, 4, 0]);
        }

        #[test]
        fn runtime_drop_abandons_successor_queued_earlier_in_control_batch() {
            let drops = Arc::new(std::sync::Mutex::new(Vec::new()));
            let calls = Arc::new(AtomicUsize::new(0));
            let tasks = Arc::new(std::sync::Mutex::new(Vec::new()));
            let effect = Effect::merge([
                Effect::concatenate([Effect::run(|_| async {}), tracked_effect(0, &drops, &calls)]),
                Effect::run(|_| async {}),
            ]);
            let (mut actor, _ingress) = launch_actor(
                effect,
                Runtime::new(LaunchDriver {
                    tasks: Arc::clone(&tasks),
                    panic_after: usize::MAX,
                }),
            );
            let (reply, _receive) = crate::store::command::commit_channel();
            let turn = actor.commit_action(ActionCommand::external((), reply));
            assert!(actor.launch(turn));
            let mut initial = std::mem::take(&mut *tasks.lock().unwrap());
            assert_eq!(initial.len(), 2);
            let abandoned = initial.pop().unwrap();
            futures_executor::block_on(initial.pop().unwrap());
            drop(abandoned);
            let turn = actor.process_control();
            assert!(!actor.launch(turn));
            assert!(tasks.lock().unwrap().is_empty());
            assert_eq!(calls.load(Ordering::Relaxed), 0);
            assert_eq!(*drops.lock().unwrap(), [0]);
        }

        #[derive(Clone, Copy)]
        enum Action {
            Stale,
            Live,
        }

        #[derive(Default)]
        struct State {
            reducer_calls: usize,
            trace: Vec<&'static str>,
        }

        struct RecordingReducer;

        impl Reducer for RecordingReducer {
            type State = State;
            type Action = Action;

            fn reduce(
                &mut self,
                state: &mut Self::State,
                action: &Self::Action,
                _context: &mut ReduceContext<'_>,
            ) -> Effect<Self::Action> {
                state.reducer_calls += 1;
                state.trace.push(match action {
                    Action::Stale => "stale",
                    Action::Live => "live",
                });
                Effect::none()
            }
        }

        /// Supplies actor checks that must never launch work.
        ///
        /// Unexpected spawning fails explicitly instead of silently discarding a task.
        #[derive(Clone, Copy)]
        struct TestDriver;

        impl fmt::Debug for TestDriver {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.debug_struct("TestDriver").finish()
            }
        }

        impl RuntimeDriver for TestDriver {
            fn spawn(&self, _task: RuntimeFuture) {
                panic!("actor fixture must not spawn work");
            }

            fn now(&self) -> MonotonicInstant {
                MonotonicInstant::ZERO
            }

            fn sleep_until(&self, deadline: MonotonicInstant) -> RuntimeFuture {
                if deadline <= self.now() {
                    Box::pin(std::future::ready(()))
                } else {
                    Box::pin(pending())
                }
            }

            fn yield_now(&self) -> RuntimeFuture {
                let mut yielded = false;
                Box::pin(std::future::poll_fn(move |context| {
                    if yielded {
                        std::task::Poll::Ready(())
                    } else {
                        yielded = true;
                        context.waker().wake_by_ref();
                        std::task::Poll::Pending
                    }
                }))
            }
        }

        #[test]
        fn test_driver_honours_frozen_clock() {
            use std::task::{Context, Waker};

            let driver = TestDriver;
            let context = &mut Context::from_waker(Waker::noop());
            let mut elapsed = driver.sleep_until(MonotonicInstant::ZERO);
            assert!(
                elapsed.as_mut().poll(context).is_ready(),
                "elapsed deadline must be ready"
            );

            for deadline in [
                MonotonicInstant::from_elapsed(std::time::Duration::from_nanos(1)),
                MonotonicInstant::MAX,
            ] {
                let mut future = driver.sleep_until(deadline);
                assert!(future.as_mut().poll(context).is_pending());
                assert_eq!(driver.now(), MonotonicInstant::ZERO);
            }
        }

        #[test]
        fn test_driver_yields_and_wakes_before_completing() {
            use std::task::{Context, Wake, Waker};

            /// Counts notifications separately from explicit future polling.
            struct CountWake(AtomicUsize);
            impl Wake for CountWake {
                fn wake(self: Arc<Self>) {
                    self.0.fetch_add(1, Ordering::SeqCst);
                }
            }

            let wake = Arc::new(CountWake(AtomicUsize::new(0)));
            let waker = Waker::from(Arc::clone(&wake));
            let context = &mut Context::from_waker(&waker);
            let mut yielding = TestDriver.yield_now();

            assert_eq!(wake.0.load(Ordering::SeqCst), 0);
            assert!(
                yielding.as_mut().poll(context).is_pending(),
                "yield must offer a scheduling turn"
            );
            assert_eq!(wake.0.load(Ordering::SeqCst), 1);
            assert!(yielding.as_mut().poll(context).is_ready());
            assert_eq!(wake.0.load(Ordering::SeqCst), 1);
        }

        #[test]
        fn stale_scope_is_rejected_immediately_before_reducer() {
            let (sender, receiver) = crate::store::mailbox::bounded(
                std::num::NonZeroUsize::new(8).expect("test capacity is nonzero"),
            );
            let control = ControlWake::new();
            let ingress = Arc::new(Ingress::new(sender, Arc::clone(&control)));
            let lifecycle = Arc::new(Lifecycle::new());
            let mut actor = Actor::new(
                State::default(),
                Box::new(RecordingReducer),
                DependencyValues::new(),
                crate::IssueReporter::default(),
                receiver,
                &ingress,
                Arc::clone(&lifecycle),
                Runtime::new(TestDriver),
                #[cfg(feature = "test-util")]
                None,
            );

            let selections = Arc::new(AtomicUsize::new(0));
            let signal = ObservationSignal::new(Arc::downgrade(&control));
            let (registration, mut registration_reply) = new_registration(
                {
                    let selections = Arc::clone(&selections);
                    move |_state: &State| {
                        selections.fetch_add(1, Ordering::AcqRel);
                    }
                },
                Arc::clone(&signal),
            );
            let turn = actor.process_released_command(Command::Observation(registration));
            assert!(actor.launch(turn));
            registration_reply
                .try_recv()
                .expect("observation registration reply remains available")
                .expect("observation registration completes");
            assert_eq!(selections.load(Ordering::Acquire), 1);

            let root = actor.scopes.root();
            let node = ScopeRegistry::node(&mut ScopeSlot::new());
            let scope = actor.scopes.mount(root, node, ScopeIdentity::unit());
            let lineage = LineageState::new(Arc::downgrade(&control));
            let prepared = actor.registry.prepare(scope, Arc::clone(&lineage), &[]);
            let task_id = prepared.id();
            let sender_token = prepared.sender_token();
            let generation_tokens = prepared.generation_tokens();
            let runtime_task = actor.registry.register(prepared, Box::pin(pending()));
            lineage.finish_registration();
            let causal_task = ActionTask::live(Arc::clone(&lineage));

            let mut transitions = ScopeTransitionBatch::default();
            transitions.extend(actor.scopes.leave(scope));
            actor.scopes.invalidate_outputs(&transitions);
            assert!(actor.registry.cancel_scopes(&transitions).is_empty());
            assert!(actor.registry.task_is_running(task_id));

            let (reply, mut receive) = crate::store::command::commit_channel();
            let command = ActionCommand::effect(
                Action::Stale,
                reply,
                scope,
                Arc::clone(&lineage),
                sender_token,
                generation_tokens,
                crate::effect::operation::OperationAncestry::default(),
                crate::effect::operation::OperationWork::default(),
            );

            let turn = actor.commit_action(command);
            assert!(!actor.launch(turn));
            assert!(matches!(
                receive.try_recv(),
                Ok(Some(Err(CommitError::Cancelled)))
            ));
            assert_eq!(actor.state.as_ref().unwrap().reducer_calls, 0);
            assert!(actor.state.as_ref().unwrap().trace.is_empty());
            assert_eq!(actor.commit_version, 0);
            assert_eq!(selections.load(Ordering::Acquire), 1);
            assert!(actor.timers.is_empty());
            assert!(!actor.observations.is_empty());

            futures_executor::block_on(runtime_task);
            let turn = actor.process_control();
            assert!(actor.launch(turn));
            futures_executor::block_on(causal_task.finish());
            assert!(causal_task.is_finished());
            assert!(!actor.registry.task_is_running(task_id));

            let (reply, mut receive) = crate::store::command::commit_channel();
            let turn = actor.commit_action(ActionCommand::external(Action::Live, reply));
            assert!(actor.launch(turn));
            let _live_task = receive
                .try_recv()
                .expect("live action sender remains available")
                .expect("live action reply completes")
                .expect("live action commits");
            assert_eq!(actor.state.as_ref().unwrap().reducer_calls, 1);
            assert_eq!(actor.state.as_ref().unwrap().trace, ["live"]);
            assert_eq!(actor.commit_version, 1);
            assert_eq!(selections.load(Ordering::Acquire), 2);
            assert!(actor.timers.is_empty());

            drop(actor);
            assert!(!signal.is_active());
            assert!(!lifecycle.was_graceful());
        }
    }
}

#[cfg(test)]
/// Routes legacy reducer fixtures through production work collection without eager polling.
struct LegacyDriver<S, A>(Box<dyn ErasedReducer<S, A>>);
#[cfg(test)]
impl<S, A> ReduceDriver<S, A> for LegacyDriver<S, A> {
    type Effect = Effect<A>;
    type Task = crate::Task<S, A>;
    fn reduce(
        &mut self,
        state: &mut S,
        action: &A,
        mut context: ReduceContext<'_>,
    ) -> crate::feature::Work<Self::Effect, Self::Task> {
        let effect = self.0.reduce(state, action, &mut context);
        let mut work = crate::feature::Work::default();
        if !matches!(effect.program, Program::None) {
            work.push(crate::feature::WorkItem::Effect(effect));
        }
        work
    }
}
