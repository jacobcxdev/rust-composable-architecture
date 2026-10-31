//! Describes owned reducer work without launching it during construction.
//!
//! Programs retain actions, captures and structural authority until the Store
//! interprets them after commit. Shared and Local leaves use the same program
//! structure with different capture bounds. Action mapping remains eager for
//! immediate sends and deferred for asynchronous output.

mod cancellation;
pub(crate) mod fallible;
pub(crate) mod leaves;
pub(crate) mod lineage;
pub(crate) mod map;
pub(crate) mod operation;
pub(crate) mod registry;
pub(crate) mod timer;

use std::collections::VecDeque;
use std::fmt;
use std::hash::Hash;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

pub(crate) use cancellation::CancellationIdentity;

#[doc(inline)]
pub use fallible::EffectError;
pub use operation::{
    CancellingOperationWait, OperationPolicy, OperationStatus, OperationWait, SelfWait,
};

use crate::store::command::{CommitError, CommitReceiver};
use crate::store::scope::ScopeToken;
use crate::{EffectSendError, MonotonicInstant, RuntimeFuture};

/// Owns a full-mailbox admission wait, ending when the command transfers or fails.
pub(crate) type SharedAdmissionFuture =
    Pin<Box<dyn Future<Output = Result<CommitReceiver, EffectSendError>> + Send + 'static>>;

/// Advances one effect send from optional capacity waiting to commit acknowledgement.
///
/// Shared and Local senders supply different admission futures. Neither family
/// retains that future after successful admission or waits for causal descendants.
pub(crate) enum EffectSendFuture<F = SharedAdmissionFuture> {
    /// The actor owns the command; this receiver observes its commit or rejection.
    ///
    /// Public send still borrows its callback chain, retaining the original
    /// lineage. No ingress owner is needed to wait for acknowledgement.
    Receiving(CommitReceiver),
    /// The future still owns the command until mailbox capacity becomes available.
    Admitting(F),
    /// Causal authority expired before admission started.
    Cancelled,
    /// The Store could not accept the command before admission started.
    StoreStopped,
}

impl<F: Future<Output = Result<CommitReceiver, EffectSendError>> + Unpin> Future
    for EffectSendFuture<F>
{
    type Output = Result<(), EffectSendError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        loop {
            match this {
                Self::Receiving(receive) => {
                    return Pin::new(receive).poll(cx).map(|result| match result {
                        Ok(Ok(_task)) => Ok(()),
                        Ok(Err(CommitError::Cancelled | CommitError::ScopeEnded)) => {
                            Err(EffectSendError::cancelled())
                        }
                        Ok(Err(CommitError::InputClosed)) => Err(EffectSendError::input_closed()),
                        Err(_closed) => Err(EffectSendError::store_stopped()),
                    });
                }
                Self::Admitting(future) => {
                    let receive = std::task::ready!(Pin::new(future).poll(cx))?;
                    // Admission is complete. Drop its pinned box before waiting
                    // for commit; at most one transition occurs in this poll.
                    *this = Self::Receiving(receive);
                }
                Self::Cancelled => return Poll::Ready(Err(EffectSendError::cancelled())),
                Self::StoreStopped => return Poll::Ready(Err(EffectSendError::store_stopped())),
            }
        }
    }
}

/// Shares an admission callback whose returned future owns its per-send inputs.
type SendOperation<Action> = dyn Fn(Action) -> EffectSendFuture + Send + Sync + 'static;

/// Owns a deferred factory; invocation constructs its independently pinned future.
pub(crate) type Run<Action> =
    Box<dyn FnOnce(EffectSender<Action>) -> RuntimeFuture + Send + 'static>;

/// Caps speculative merge allocation at the supported static Combine width.
///
/// Larger dynamic merges grow with retained children instead of trusting an
/// arbitrary iterator size hint.
const MAX_MERGE_PREALLOCATION: usize = 12;

/// Reserves for the first retained child and a bounded number of remaining inputs.
fn initial_merge_capacity(remaining_lower_bound: usize) -> usize {
    remaining_lower_bound
        .saturating_add(1)
        .min(MAX_MERGE_PREALLOCATION)
}

/// Sends actions from one running effect.
///
/// Clones share the callback and its causal authority, not Store ownership.
/// Cancellation and closure can reject later sends even while a sender is retained.
pub struct EffectSender<Action> {
    /// Retains the routing callback and its provenance across borrowed send futures.
    send: Arc<SendOperation<Action>>,
}

impl<Action> EffectSender<Action> {
    /// Erases one routing callback without allocating a separate future per forwarding step.
    pub(crate) fn new(send: impl Fn(Action) -> EffectSendFuture + Send + Sync + 'static) -> Self {
        Self {
            send: Arc::new(send),
        }
    }

    /// Sends an action and waits for its commit.
    ///
    /// Admission starts when this future is polled. Dropping it before admission
    /// discards the unadmitted action; dropping it after admission does not retract
    /// the command. Successful completion does not wait for descendant effects.
    ///
    /// # Errors
    ///
    /// Returns [`EffectSendError`] when cancellation, input closure or Store termination
    /// rejects delivery.
    pub async fn send(&self, action: Action) -> Result<(), EffectSendError> {
        (self.send)(action).await
    }
}

impl<Action> Clone for EffectSender<Action> {
    fn clone(&self) -> Self {
        Self {
            send: Arc::clone(&self.send),
        }
    }
}

impl<Action> fmt::Debug for EffectSender<Action> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EffectSender").finish_non_exhaustive()
    }
}

/// Describes work performed after reduction.
///
/// Construction may retain borrowed or non-`Send` actions. Factories and futures
/// keep their own owned, `Send` capture bounds. Shared Store admission requires
/// `Send + 'static` actions; [`Self::map`] can convert owned local outputs before
/// that boundary. Creating an effect does not launch it.
pub struct Effect<Action> {
    /// Owns the unexecuted work, including payloads and recursive continuations.
    pub(crate) program: Program<Action>,
    /// Overrides inherited structural authority when composition assigns an owner.
    pub(crate) scope: Option<ScopeToken>,
}

/// Retains scheduling and authority declarations independently of leaf thread bounds.
///
/// The actor interprets wrappers in declaration order. A wrapper around empty
/// work may still register or replace authority, so it is not generally empty.
pub(crate) enum Program<Action, C: leaves::EffectLeaves<Action> = leaves::SharedEffects> {
    /// No work and no registration side effects.
    None,
    /// Requests structural dismissal through the matching composition adapter.
    Dismiss(crate::composition::DismissTarget),
    /// Owns an action whose admission begins after the triggering commit.
    Send(Action),
    /// Defers factory invocation until managed work starts polling.
    Run(C::Run),
    /// Defers work that needs live authority when routing its typed failure.
    FallibleRun(C::FallibleRun),
    /// Owns an existing future producing at most one action, without polling it here.
    Future(C::Future),
    /// Registers retained children in source order without waiting between them.
    Merge(Vec<C::Effect>),
    /// Owns the current child's successors until completion releases each next child.
    Concatenate(VecDeque<C::Effect>),
    /// Removes inherited cancellation generations while retaining other ownership.
    WithoutInheritedCancellation(Box<C::Effect>),
    /// Cancels matching live generations in the interpreting reducer scope.
    Cancel(CancellationIdentity),
    /// Prefixes a delayed continuation without producing an action itself.
    Wait(Duration),
    /// Retains a one-shot action until a relative timer expires.
    SendAfter {
        /// Starts when the actor interprets this program, using saturating time arithmetic.
        duration: Duration,
        /// Released on cancellation or transferred to admission when the timer is due.
        action: Action,
    },
    /// Retains a one-shot action against an unchanged absolute monotonic deadline.
    SendAt {
        /// Uses the target runtime's clock origin; sequencing does not rebase it.
        deadline: MonotonicInstant,
        /// Released on cancellation or transferred to admission when the timer is due.
        action: Action,
    },
    /// Reuses a retained factory to produce fixed-rate timer actions.
    Interval {
        /// Nonzero cadence established by the constructor.
        period: Duration,
        /// Invoked only for due ticks, never during construction or Debug formatting.
        make_action: C::Interval,
    },
    /// Registers a typed cancellation generation around its nested program.
    Cancellable {
        /// Keeps its existing structural scope and all unexecuted payloads.
        effect: Box<C::Effect>,
        /// Receives its reducer scope when the actor interprets this wrapper.
        identity: CancellationIdentity,
        /// Cancels earlier matching generations before registering this one.
        cancel_in_flight: bool,
    },
    /// Tracks nested work and causal descendants in the named-operation namespace.
    Named {
        /// Retains the work whose registration is bracketed by the named generation.
        effect: Box<C::Effect>,
        /// Owns the typed key and registration-time coexistence or replacement policy.
        declaration: operation::OperationDeclaration,
    },
}

impl<Action> Effect<Action> {
    /// Tracks this work and its causal descendants under a typed operation key.
    ///
    /// Named keys have a separate identity domain from ordinary cancellation
    /// keys. `Replace` cancels previous generations without hiding retained work.
    /// Keys obey the stable identity law of [`Self::cancellable`] from this call
    /// until the declaration and all registered work release them.
    #[must_use]
    pub fn named<K>(self, key: K, policy: OperationPolicy) -> Self
    where
        K: Hash + Eq + Send + Sync + 'static,
    {
        Self {
            program: Program::Named {
                effect: Box::new(self),
                declaration: operation::OperationDeclaration::new(key, policy),
            },
            scope: None,
        }
    }

    /// Maps each emitted action into another action type.
    ///
    /// Source and mapped actions may be owned non-`Send` values. Both action
    /// types must be `'static`: this concrete effect representation retains
    /// owned callbacks so mapping covers immediate and deferred work uniformly.
    /// Immediate sends are mapped synchronously; nonempty effects need not be
    /// asynchronous. The mapper itself remains `Send + Sync + 'static`.
    ///
    /// This is a boundary of this representation, not of local synchronous
    /// [`crate::Reducer`] action borrowing. Asynchronous captures and actor
    /// admission retain their own `Send + 'static` requirements.
    #[must_use]
    pub fn map<Mapped, Map>(self, map: Map) -> Effect<Mapped>
    where
        Action: 'static,
        Mapped: 'static,
        Map: Fn(Action) -> Mapped + Send + Sync + 'static,
    {
        map::map_effect(self, map)
    }

    /// Produces no work.
    #[must_use]
    pub const fn none() -> Self {
        Self {
            program: Program::None,
            scope: None,
        }
    }

    /// Merges non-empty effects in source order for concurrent execution.
    ///
    /// Consumes the iterator synchronously without capturing its borrows.
    /// Returns [`Effect::none`] without allocating when every effect is empty.
    /// Effect execution retains its actor transport and owned capture bounds.
    pub fn merge(effects: impl IntoIterator<Item = Self>) -> Self {
        leaves::merge::<Action, leaves::SharedEffects>(effects)
    }

    /// Tags unowned nonempty work without replacing an already selected child scope.
    pub(crate) fn in_scope(mut self, token: ScopeToken) -> Self {
        if !matches!(self.program, Program::None) && self.scope.is_none() {
            self.scope = Some(token);
        }
        self
    }

    /// Excludes cancellation identities inherited from the triggering action.
    ///
    /// Explicit cancellation wrappers on this effect remain effective, regardless
    /// of modifier order. Root action-task cancellation, structural scope removal
    /// and Store shutdown still own this work. This does not detach a task or
    /// allow an obsolete triggering action to pass Store admission.
    ///
    /// Wrap an entire sequence to transfer its continuation. Wrapping only one
    /// element does not protect the enclosing sequence or its later elements.
    #[must_use]
    pub fn without_inherited_cancellation(self) -> Self {
        if matches!(self.program, Program::None) {
            return self;
        }
        Self {
            scope: self.scope,
            program: Program::WithoutInheritedCancellation(Box::new(self)),
        }
    }

    /// Sends one action after commit.
    pub fn send(action: Action) -> Self {
        Self {
            program: Program::Send(action),
            scope: None,
        }
    }

    /// Awaits one optional action after commit.
    ///
    /// Takes ownership of the already-created future immediately, but does not
    /// poll it until the effect launches. Queued concatenations retain the
    /// future; cancellation or discarding an unlaunched effect drops it without
    /// polling. Use [`Effect::run`] when construction itself must be deferred.
    ///
    /// Drops the completed future before delivering its result. `Some(action)`
    /// waits for the action's commit; `None` completes without sending. Like
    /// [`Effect::run`], cancellation cannot pre-empt a non-yielding poll.
    pub fn future<Fut>(future: Fut) -> Self
    where
        Fut: Future<Output = Option<Action>> + Send + 'static,
    {
        Self {
            program: Program::<Action>::Future(Box::pin(future)),
            scope: None,
        }
    }

    /// Runs asynchronous work after commit.
    ///
    /// Owns the factory immediately, but invokes it only when the effect launches.
    /// Discarding an unlaunched effect drops its captures without invoking it.
    /// Use [`Self::future`] when the future has already been constructed.
    ///
    /// Runtime cancellation cannot pre-empt a future that spins forever in one
    /// poll. Long-running work must await or yield so cancellation and Store
    /// shutdown can make progress.
    pub fn run<F, Fut>(operation: F) -> Self
    where
        F: FnOnce(EffectSender<Action>) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        Self {
            program: Program::<Action>::Run(Box::new(move |sender| Box::pin(operation(sender)))),
            scope: None,
        }
    }

    /// Starts each effect after its predecessor finishes.
    ///
    /// Consumes the iterator synchronously and retains all children, including
    /// already-created futures. Factories remain deferred until their turn.
    /// Each child and its causal descendants finish before the next starts;
    /// cancellation can discard the unstarted suffix without invoking its factories.
    pub fn concatenate(effects: impl IntoIterator<Item = Self>) -> Self {
        Self {
            program: Program::Concatenate(effects.into_iter().collect()),
            scope: None,
        }
    }

    /// Cancels every live generation matching `id` in this reducer scope.
    ///
    /// Keys must obey the identity stability law of [`Self::cancellable`],
    /// starting when they are passed to this constructor.
    pub fn cancel<ID>(id: ID) -> Self
    where
        ID: Hash + Eq + Send + Sync + 'static,
    {
        Self {
            program: Program::Cancel(CancellationIdentity::local(id)),
            scope: None,
        }
    }

    /// Sends one action no earlier than `duration` after commit.
    pub fn send_after(duration: Duration, action: Action) -> Self {
        Self {
            program: Program::SendAfter { duration, action },
            scope: None,
        }
    }

    /// Sends one action no earlier than an absolute monotonic deadline.
    ///
    /// The deadline must use the Store runtime's clock origin, for example a
    /// value derived from its [`crate::Runtime::now`]. Clock origins are not
    /// checked or translated. This is monotonic time, not wall-clock time.
    ///
    /// Delaying or sequencing this effect does not rebase the deadline. An
    /// elapsed deadline is eligible when the Store reaches this program,
    /// through the normal actor queue; delivery may occur later. A deadline of
    /// [`MonotonicInstant::MAX`] stays pending until cancelled or shut down.
    /// Mapping is eager and preserves the deadline. Root tasks, cancellation
    /// IDs and structural scope retain ownership just as with [`Self::send_after`].
    ///
    /// # Examples
    ///
    /// ```
    /// use rca_core::{Effect, Runtime};
    /// use std::time::Duration;
    /// fn deadline_effect(runtime: &Runtime) -> Effect<u8> {
    ///     Effect::send_at(runtime.now().saturating_add(Duration::from_secs(1)), 42)
    /// }
    /// ```
    pub fn send_at(deadline: MonotonicInstant, action: Action) -> Self {
        Self {
            program: Program::SendAt { deadline, action },
            scope: None,
        }
    }

    /// Delays execution of this effect program.
    ///
    /// Waiting starts when the Store reaches this program. Effect construction
    /// and mapping remain eager; put external operations inside [`Self::run`].
    /// Zero duration returns this effect unchanged; an empty effect stays empty.
    /// Positive durations use the Store's saturating monotonic timer deadlines.
    ///
    /// Cancellation wrappers outside the delay own both waiting and execution.
    /// Wrappers inside register only after waiting. For debounce, use
    /// `effect.delay(duration).cancellable_in_flight(id)`. Root task cancellation,
    /// structural removal and Store shutdown retain ownership of both phases.
    /// A surrounding sequence waits for the delayed child, not only its timer.
    ///
    /// An inherited-cancellation boundary inside the delay cannot rescue an
    /// already cancelled wait; put the boundary outside to transfer both phases.
    ///
    /// # Examples
    ///
    /// ```
    /// use rca_core::Effect;
    /// use std::time::Duration;
    /// let effect = Effect::run(|sender| async move {
    ///     let _ = sender.send(42).await;
    /// })
    /// .delay(Duration::from_millis(100))
    /// .cancellable_in_flight("search");
    /// # drop(effect);
    /// ```
    #[must_use]
    pub fn delay(self, duration: Duration) -> Self {
        if duration.is_zero() || matches!(self.program, Program::None) {
            return self;
        }
        let scope = self.scope;
        let wait = Self {
            program: Program::Wait(duration),
            scope: None,
        };
        Self {
            program: Program::Concatenate([wait, self].into()),
            scope,
        }
    }

    /// Generates fixed-rate actions until cancelled.
    ///
    /// # Panics
    ///
    /// Panics when `period` is zero.
    pub fn interval<F>(period: Duration, make_action: F) -> Self
    where
        F: FnMut() -> Action + Send + 'static,
    {
        assert!(!period.is_zero(), "interval period must be non-zero");
        Self {
            program: Program::<Action>::Interval {
                period,
                make_action: Box::new(make_action),
            },
            scope: None,
        }
    }

    /// Associates this effect with one typed cancellation identity.
    ///
    /// Independently created equal keys of the same Rust type match within the
    /// current reducer scope. Other types and reducer scopes remain distinct.
    ///
    /// The caller must keep `Eq` and `Hash` coherent and the key's identity stable
    /// from this call until the resulting effect and its registered work release
    /// the key. Owning a key does not freeze shared interior state: the hash is
    /// cached at construction, but equality still observes the stored value.
    /// Unrelated mutable metadata may change only when it cannot affect `Eq` or
    /// `Hash`. Changing identity during this interval is invalid input; lookup
    /// and cancellation behaviour are not guaranteed for such keys.
    #[must_use]
    pub fn cancellable<ID>(self, id: ID) -> Self
    where
        ID: Hash + Eq + Send + Sync + 'static,
    {
        Self {
            program: Program::Cancellable {
                effect: Box::new(self),
                identity: CancellationIdentity::local(id),
                cancel_in_flight: false,
            },
            scope: None,
        }
    }

    /// Replaces live effects sharing one typed cancellation identity.
    ///
    /// Keys must obey the identity stability law of [`Self::cancellable`],
    /// starting when they are passed to this constructor.
    #[must_use]
    pub fn cancellable_in_flight<ID>(self, id: ID) -> Self
    where
        ID: Hash + Eq + Send + Sync + 'static,
    {
        Self {
            program: Program::Cancellable {
                effect: Box::new(self),
                identity: CancellationIdentity::local(id),
                cancel_in_flight: true,
            },
            scope: None,
        }
    }
}

impl<Action> Default for Effect<Action> {
    fn default() -> Self {
        Self::none()
    }
}

impl<Action> fmt::Debug for Effect<Action> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.program {
            Program::None => f.write_str("Effect::None"),
            Program::Dismiss(_) => f.write_str("Effect::Dismiss"),
            Program::Send(_) => f.write_str("Effect::Send"),
            Program::Run(_) => f.write_str("Effect::Run"),
            Program::FallibleRun(_) => f.write_str("Effect::FallibleRun"),
            Program::Future(_) => f.write_str("Effect::Future"),
            Program::Merge(effects) => write!(f, "Effect::Merge({})", effects.len()),
            Program::Concatenate(effects) => {
                write!(f, "Effect::Concatenate({})", effects.len())
            }
            Program::Cancel(_) => f.write_str("Effect::Cancel"),
            Program::Wait(_) => f.write_str("Effect::Wait"),
            Program::WithoutInheritedCancellation(_) => {
                f.write_str("Effect::WithoutInheritedCancellation")
            }
            Program::SendAfter { .. } => f.write_str("Effect::SendAfter"),
            Program::SendAt { .. } => f.write_str("Effect::SendAt"),
            Program::Interval { .. } => f.write_str("Effect::Interval"),
            Program::Cancellable {
                cancel_in_flight: true,
                ..
            } => f.write_str("Effect::CancellableInFlight"),
            Program::Cancellable {
                cancel_in_flight: false,
                ..
            } => f.write_str("Effect::Cancellable"),
            Program::Named { .. } => f.write_str("Effect::Named"),
        }
    }
}

#[cfg(all(test, feature = "allocation-measurement"))]
mod allocation_tests {
    use std::hint::black_box;

    use super::Effect;
    use crate::store::scope::ScopeRegistry;

    pub(crate) fn scope_tagging_non_empty_effects_does_not_allocate() {
        if crate::allocation_test::run_in_child(
            "effect::allocation_tests::scope_tagging_non_empty_effects_does_not_allocate",
        ) {
            return;
        }
        let token = ScopeRegistry::for_test().1.root();
        let send = Effect::send(1_u8);
        let run = Effect::<u8>::run(|_| std::future::pending());
        let profiler = dhat::Profiler::builder().testing().build();
        let before = dhat::HeapStats::get();

        let send = black_box(send.in_scope(token));
        let run = black_box(run.in_scope(token));

        let after = dhat::HeapStats::get();
        assert_eq!(after.total_blocks, before.total_blocks);
        assert_eq!(after.total_bytes, before.total_bytes);
        drop((send, run));
        drop(profiler);
    }
}

#[cfg(test)]
ordinary_test_items! {
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::leaves::EffectValue;
    use super::*;
    use crate::store::scope::ScopeRegistry;

    /// Tracks payload release without supplying Debug or Clone implementations.
    struct DropCount(Arc<AtomicUsize>);

    impl Drop for DropCount {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn debug_reports_work_kind_without_observing_payloads_or_running_work() {
        // Formatting may identify work, but must neither execute it nor require
        // Debug on an owned action or expose its cancellation/operation key.
        let drops = Arc::new(AtomicUsize::new(0));
        let payload = || DropCount(Arc::clone(&drops));
        let run_capture = payload();
        let future_capture = payload();
        let interval_capture = payload();
        let cases = [
            (Effect::send(payload()), "Effect::Send", 1),
            (
                Effect::run(move |_| async move {
                    let _capture = run_capture;
                    panic!("formatting must not run an effect");
                }),
                "Effect::Run",
                1,
            ),
            (
                Effect::future(async move {
                    let _capture = future_capture;
                    panic!("formatting must not poll a future");
                }),
                "Effect::Future",
                1,
            ),
            (
                Effect::merge([Effect::send(payload()), Effect::send(payload())]),
                "Effect::Merge(2)",
                2,
            ),
            (Effect::cancel("private-key"), "Effect::Cancel", 0),
            (
                Effect::send(payload()).without_inherited_cancellation(),
                "Effect::WithoutInheritedCancellation",
                1,
            ),
            (
                Effect::send_after(Duration::from_secs(1), payload()),
                "Effect::SendAfter",
                1,
            ),
            (
                Effect::send_at(MonotonicInstant::ZERO, payload()),
                "Effect::SendAt",
                1,
            ),
            (
                Effect::interval(Duration::from_secs(1), move || {
                    let _ = &interval_capture;
                    panic!("formatting must not invoke an interval factory");
                }),
                "Effect::Interval",
                1,
            ),
            (
                Effect::send(payload()).cancellable("private-key"),
                "Effect::Cancellable",
                1,
            ),
            (
                Effect::send(payload()).named("private-key", OperationPolicy::Coexist),
                "Effect::Named",
                1,
            ),
        ];
        let mut disposed = 0;
        for (effect, expected, owned) in cases {
            assert_eq!(format!("{effect:?}"), expected);
            assert_eq!(drops.load(Ordering::Relaxed), disposed);
            drop(effect);
            disposed += owned;
            assert_eq!(drops.load(Ordering::Relaxed), disposed);
        }
        assert_eq!(disposed, 11);
    }

    #[test]
    fn pruning_compositions_preserves_live_scopes_payloads_and_order() {
        use crate::store::scope::{ScopeIdentity, ScopeSlot};

        // Pruning one owner must not erase or reorder another owner's queued work.
        for concatenate in [false, true] {
            let (_scopes_lifecycle, mut scopes) = ScopeRegistry::for_test();
            let root = scopes.root();
            let tokens: Vec<_> = (0..3)
                .map(|_| {
                    let node = ScopeRegistry::node(&mut ScopeSlot::new());
                    scopes.mount(root, node, ScopeIdentity::unit())
                })
                .collect();
            let drops: Vec<_> = (0..3).map(|_| Arc::new(AtomicUsize::new(0))).collect();
            let children = (0..3).map(|index| {
                Effect::send((index, DropCount(Arc::clone(&drops[index])))).in_scope(tokens[index])
            });
            let mut effect = if concatenate {
                Effect::concatenate(children)
            } else {
                Effect::merge(children)
            };

            assert!(!effect.prune_scopes(&|token| scopes.is_current(token)));
            assert_eq!(
                drops
                    .iter()
                    .map(|n| n.load(Ordering::Relaxed))
                    .collect::<Vec<_>>(),
                [0, 0, 0]
            );
            scopes.leave(tokens[1]);
            assert!(effect.prune_scopes(&|token| scopes.is_current(token)));
            assert_eq!(
                drops
                    .iter()
                    .map(|n| n.load(Ordering::Relaxed))
                    .collect::<Vec<_>>(),
                [0, 1, 0]
            );
            let children: Vec<_> = match &effect.program {
                Program::Merge(children) => children.iter().collect(),
                Program::Concatenate(children) => children.iter().collect(),
                _ => panic!("live children must remain queued"),
            };
            let outputs: Vec<_> = children
                .iter()
                .map(|child| {
                    let Program::Send((value, _)) = &child.program else {
                        panic!("queued payload must survive pruning");
                    };
                    (*value, child.scope)
                })
                .collect();
            assert_eq!(outputs, [(0, Some(tokens[0])), (2, Some(tokens[2]))]);
            assert!(!effect.prune_scopes(&|token| scopes.is_current(token)));

            scopes.leave(tokens[0]);
            scopes.leave(tokens[2]);
            assert!(effect.prune_scopes(&|token| scopes.is_current(token)));
            assert!(matches!(effect.program, Program::None));
            assert_eq!(
                drops
                    .iter()
                    .map(|n| n.load(Ordering::Relaxed))
                    .collect::<Vec<_>>(),
                [1, 1, 1]
            );
        }
    }

    #[test]
    fn pruning_wrappers_preserves_then_releases_scoped_captures() {
        use crate::store::scope::{ScopeIdentity, ScopeSlot};

        for inherited_boundary in [false, true] {
            let (_scopes_lifecycle, mut scopes) = ScopeRegistry::for_test();
            let node = ScopeRegistry::node(&mut ScopeSlot::new());
            let token = scopes.mount(scopes.root(), node, ScopeIdentity::unit());
            let drops = Arc::new(AtomicUsize::new(0));
            let capture = DropCount(Arc::clone(&drops));
            let effect = Effect::<()>::run(move |_| async move { drop(capture) }).in_scope(token);
            let mut effect = if inherited_boundary {
                effect.without_inherited_cancellation()
            } else {
                effect.cancellable("scoped work")
            };

            assert!(!effect.prune_scopes(&|scope| scopes.is_current(scope)));
            assert_eq!(drops.load(Ordering::Relaxed), 0);
            scopes.leave(token);
            assert!(effect.prune_scopes(&|scope| scopes.is_current(scope)));
            assert_eq!(drops.load(Ordering::Relaxed), 1);
            assert!(matches!(effect.program, Program::None));
            drop(effect);
            assert_eq!(drops.load(Ordering::Relaxed), 1);
        }
    }

    #[test]
    fn pruning_named_work_retains_live_descendants_until_their_scope_retires() {
        use crate::store::scope::{ScopeIdentity, ScopeSlot};

        let (_scopes_lifecycle, mut scopes) = ScopeRegistry::for_test();
        let root = scopes.root();
        let retired_node = ScopeRegistry::node(&mut ScopeSlot::new());
        let retired = scopes.mount(root, retired_node, ScopeIdentity::unit());
        let live_node = ScopeRegistry::node(&mut ScopeSlot::new());
        let live = scopes.mount(root, live_node, ScopeIdentity::unit());
        let retired_drops = Arc::new(AtomicUsize::new(0));
        let live_drops = Arc::new(AtomicUsize::new(0));
        let mut named = Effect::merge([
            Effect::send((7, DropCount(Arc::clone(&retired_drops)))).in_scope(retired),
            Effect::send((11, DropCount(Arc::clone(&live_drops)))).in_scope(live),
        ])
        .named("queued work", OperationPolicy::Coexist);

        assert!(!named.prune_scopes(&|scope| scopes.is_current(scope)));
        assert_eq!(retired_drops.load(Ordering::Relaxed), 0);
        assert_eq!(live_drops.load(Ordering::Relaxed), 0);

        scopes.leave(retired);
        assert!(named.prune_scopes(&|scope| scopes.is_current(scope)));
        assert_eq!(retired_drops.load(Ordering::Relaxed), 1);
        assert_eq!(live_drops.load(Ordering::Relaxed), 0);
        let Program::Named { effect, .. } = &named.program else {
            panic!("live work must retain its named operation");
        };
        let Program::Merge(children) = &effect.program else {
            panic!("the live descendant must retain its own scope");
        };
        assert_eq!(children.len(), 1);
        let Program::Send((value, _)) = &children[0].program else {
            panic!("the live payload must remain queued");
        };
        assert_eq!(*value, 11);
        assert_eq!(children[0].scope, Some(live));
        assert!(!named.prune_scopes(&|scope| scopes.is_current(scope)));

        scopes.leave(live);
        assert!(named.prune_scopes(&|scope| scopes.is_current(scope)));
        assert!(matches!(named.program, Program::None));
        assert_eq!(live_drops.load(Ordering::Relaxed), 1);
        drop(named);
        assert_eq!(retired_drops.load(Ordering::Relaxed), 1);
        assert_eq!(live_drops.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn delayed_wait_diagnostics_preserve_queued_payload_ownership() {
        let drops = Arc::new(AtomicUsize::new(0));
        let delayed = Effect::send(DropCount(Arc::clone(&drops))).delay(Duration::from_secs(1));
        let Program::Concatenate(children) = &delayed.program else {
            panic!("positive delay must retain its wait and payload");
        };
        assert_eq!(children.len(), 2);
        assert_eq!(
            format!("{:?}", children.front().expect("delay begins with a wait")),
            "Effect::Wait"
        );
        assert_eq!(drops.load(Ordering::Relaxed), 0);
        drop(delayed);
        assert_eq!(drops.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn merge_retains_scoped_owned_children_until_drop() {
        for count in [1, 2] {
            let drops: Vec<_> = (0..count).map(|_| Arc::new(AtomicUsize::new(0))).collect();
            let token = ScopeRegistry::for_test().1.root();
            let mut effects = vec![Effect::none()];
            for counter in &drops {
                effects.push(Effect::send(DropCount(Arc::clone(counter))).in_scope(token));
                effects.push(Effect::none());
            }
            let merged = Effect::merge(effects);

            assert!(merged.scope.is_none());
            let Program::Merge(children) = &merged.program else {
                panic!("even a singleton must retain its outer merge authority");
            };
            assert_eq!(children.len(), count);
            for child in children {
                assert_eq!(child.scope, Some(token));
                assert!(matches!(child.program, Program::Send(_)));
            }
            for counter in &drops {
                assert_eq!(counter.load(Ordering::Relaxed), 0);
            }
            drop(merged);
            for counter in &drops {
                assert_eq!(counter.load(Ordering::Relaxed), 1);
            }
        }
    }

    #[test]
    fn merge_iterator_unwind_drops_retained_and_unconsumed_payloads_once() {
        let retained = Arc::new(AtomicUsize::new(0));
        let retained_tail = Arc::new(AtomicUsize::new(0));
        let unconsumed = Arc::new(AtomicUsize::new(0));
        let effects = [
            Effect::send(DropCount(Arc::clone(&retained))),
            Effect::none(),
            Effect::send(DropCount(Arc::clone(&retained_tail))),
            Effect::none(),
            Effect::none(),
            Effect::send(DropCount(Arc::clone(&unconsumed))),
        ];
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            Effect::merge(effects.into_iter().enumerate().map(|(index, effect)| {
                assert_ne!(index, 4, "iterator panic after retaining two children");
                effect
            }))
        }));

        result.expect_err("iterator must panic before consuming its tail");
        assert_eq!(retained.load(Ordering::Relaxed), 1);
        assert_eq!(retained_tail.load(Ordering::Relaxed), 1);
        assert_eq!(unconsumed.load(Ordering::Relaxed), 1);
    }

    /// A hint panic during growth must release both retained and unconsumed children.
    #[test]
    fn merge_growth_hint_panic_drops_every_payload_once() {
        /// Allows initial reservation, then panics when growth asks for another hint.
        struct HintPanics {
            /// Owns every source child until iteration transfers it.
            effects: std::vec::IntoIter<Effect<DropCount>>,
            /// Counts size-hint calls without changing the iterator position.
            hints: std::cell::Cell<usize>,
            /// Counts children released before iterator cleanup starts.
            drops: Arc<AtomicUsize>,
            /// Records cleanup order without panicking during unwinding.
            drops_before_cleanup: Arc<AtomicUsize>,
        }

        impl Iterator for HintPanics {
            type Item = Effect<DropCount>;

            fn next(&mut self) -> Option<Self::Item> {
                self.effects.next()
            }

            fn size_hint(&self) -> (usize, Option<usize>) {
                let calls = self.hints.get();
                self.hints.set(calls + 1);
                assert_eq!(calls, 0, "growth hint must panic");
                self.effects.size_hint()
            }
        }

        impl Drop for HintPanics {
            fn drop(&mut self) {
                self.drops_before_cleanup
                    .store(self.drops.load(Ordering::Relaxed), Ordering::Relaxed);
            }
        }

        let drops = Arc::new(AtomicUsize::new(0));
        let drops_before_cleanup = Arc::new(AtomicUsize::new(usize::MAX));
        let effects = HintPanics {
            effects: (0..24)
                .map(|_| Effect::send(DropCount(Arc::clone(&drops))))
                .collect::<Vec<_>>()
                .into_iter(),
            hints: std::cell::Cell::new(0),
            drops: Arc::clone(&drops),
            drops_before_cleanup: Arc::clone(&drops_before_cleanup),
        };
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| Effect::merge(effects)));

        result.expect_err("growth must ask for the remaining upper hint");
        assert_eq!(drops_before_cleanup.load(Ordering::Relaxed), 1);
        assert_eq!(drops.load(Ordering::Relaxed), 24);
    }

    /// Iterator cleanup runs while collected children remain owned by the merge builder.
    #[test]
    fn merge_iterator_cleanup_panic_releases_collected_payloads_once() {
        /// Reports its final state before panicking during cleanup.
        struct CleanupPanics {
            /// Owns source children until they have all been transferred.
            effects: std::vec::IntoIter<Effect<DropCount>>,
            /// Counts retained children released by the builder's unwind.
            drops: Arc<AtomicUsize>,
            /// Records the iterator's state without risking a second panic.
            clean: Arc<std::sync::atomic::AtomicBool>,
        }

        impl Iterator for CleanupPanics {
            type Item = Effect<DropCount>;

            fn next(&mut self) -> Option<Self::Item> {
                self.effects.next()
            }

            fn size_hint(&self) -> (usize, Option<usize>) {
                self.effects.size_hint()
            }
        }

        impl Drop for CleanupPanics {
            fn drop(&mut self) {
                self.clean.store(
                    self.effects.as_slice().is_empty() && self.drops.load(Ordering::Relaxed) == 0,
                    Ordering::Relaxed,
                );
                assert!(std::thread::panicking(), "iterator cleanup panic");
            }
        }

        let drops = Arc::new(AtomicUsize::new(0));
        let clean = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let effects = CleanupPanics {
            effects: (0..24)
                .map(|_| Effect::send(DropCount(Arc::clone(&drops))))
                .collect::<Vec<_>>()
                .into_iter(),
            drops: Arc::clone(&drops),
            clean: Arc::clone(&clean),
        };
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| Effect::merge(effects)));

        result.expect_err("iterator cleanup must panic");
        assert!(clean.load(Ordering::Relaxed));
        assert_eq!(drops.load(Ordering::Relaxed), 24);
    }

    /// Separates retained output from the iterator hints used for reservation.
    struct HintedEffects {
        effects: VecDeque<Effect<u8>>,
        lower: usize,
        upper: Option<usize>,
    }

    impl HintedEffects {
        fn new(effects: Vec<Effect<u8>>, lower: usize, upper: Option<usize>) -> Self {
            Self {
                effects: effects.into(),
                lower,
                upper,
            }
        }
    }

    impl Iterator for HintedEffects {
        type Item = Effect<u8>;

        fn next(&mut self) -> Option<Self::Item> {
            self.effects.pop_front()
        }

        fn size_hint(&self) -> (usize, Option<usize>) {
            (self.lower.min(self.effects.len()), self.upper)
        }
    }

    /// Reads retained payload order and capacity without running the effect program.
    fn merge_outputs(effects: impl IntoIterator<Item = Effect<u8>>) -> (Vec<u8>, usize) {
        let effect = Effect::merge(effects);
        let Program::Merge(effects) = effect.program else {
            panic!("non-empty input must produce a merge");
        };
        let capacity = effects.capacity();
        let outputs = effects
            .into_iter()
            .map(|effect| match effect.program {
                Program::Send(output) => output,
                _ => panic!("retained children must be sends"),
            })
            .collect();
        (outputs, capacity)
    }

    #[test]
    fn merge_ignores_huge_loose_upper_hint() {
        let effects =
            HintedEffects::new(vec![Effect::send(1)], 0, Some(usize::MAX.saturating_sub(1)));

        assert_eq!(merge_outputs(effects), (vec![1], 1));
    }

    #[test]
    fn merge_uses_non_exact_lower_hint_in_source_order() {
        let effects = HintedEffects::new(vec![Effect::send(1), Effect::send(2)], 1, Some(16));

        assert_eq!(merge_outputs(effects), (vec![1, 2], 2));
    }

    #[test]
    fn merge_handles_overflowing_upper_hint() {
        let effects = HintedEffects::new(vec![Effect::send(1)], 0, Some(usize::MAX));

        assert_eq!(merge_outputs(effects), (vec![1], 1));
    }

    #[test]
    fn merge_bounds_filter_like_retained_output_capacity() {
        let effects = HintedEffects::new(
            vec![
                Effect::none(),
                Effect::send(1),
                Effect::none(),
                Effect::send(2),
                Effect::none(),
            ],
            0,
            Some(1_024),
        );

        let (outputs, capacity) = merge_outputs(effects);
        assert_eq!(outputs, [1, 2]);
        assert!(capacity <= 4, "capacity {capacity} exceeded retained input");
    }

    #[test]
    fn merge_bounds_truthful_exact_trailing_none_capacity() {
        const TRAILING_NONE_COUNT: usize = 128;

        let mut effects = vec![Effect::send(1), Effect::send(2)];
        effects.extend(std::iter::repeat_with(Effect::none).take(TRAILING_NONE_COUNT));

        let (outputs, capacity) = merge_outputs(effects);
        assert_eq!(outputs, [1, 2]);
        assert_eq!(capacity, MAX_MERGE_PREALLOCATION);
    }

    /// Bounds dense growth by the number of source items still available.
    #[test]
    fn merge_dense_growth_does_not_overshoot_remaining_input() {
        let (outputs, capacity) = merge_outputs((0..100).map(Effect::send));

        assert_eq!(outputs, (0..100).collect::<Vec<_>>());
        assert!(capacity <= 100, "capacity {capacity} exceeded dense input");
    }

    /// Unknown and excessive upper hints cannot force speculative reservation.
    #[test]
    fn merge_growth_bounds_unknown_and_excessive_upper_hints() {
        for upper in [None, Some(usize::MAX)] {
            let effects = HintedEffects::new((0..100).map(Effect::send).collect(), 0, upper);
            let (outputs, capacity) = merge_outputs(effects);

            assert_eq!(outputs, (0..100).collect::<Vec<_>>());
            assert!(capacity <= 128, "capacity {capacity} exceeded bounded growth");
        }
    }

    #[test]
    fn merge_capacity_policy_caps_near_overflow_lower_hints() {
        assert_eq!(initial_merge_capacity(0), 1);
        assert_eq!(
            initial_merge_capacity(MAX_MERGE_PREALLOCATION - 1),
            MAX_MERGE_PREALLOCATION
        );
        assert_eq!(
            initial_merge_capacity(MAX_MERGE_PREALLOCATION),
            MAX_MERGE_PREALLOCATION
        );
        assert_eq!(
            initial_merge_capacity(usize::MAX - 1),
            MAX_MERGE_PREALLOCATION
        );
        assert_eq!(initial_merge_capacity(usize::MAX), MAX_MERGE_PREALLOCATION);
    }
}
}

#[cfg(all(test, feature = "allocation-measurement"))]
pub(crate) use allocation_tests::scope_tagging_non_empty_effects_does_not_allocate;
