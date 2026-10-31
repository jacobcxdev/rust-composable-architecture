//! Checks bounded actor admission, commit ordering and terminal resource ownership.
//!
//! Panic children mark success only after their lifecycle assertions have passed.

mod support;

/// Reuses bounded capture and exact child selection; no actor-specific process runner.
mod subprocess {
    include!("../src/allocation_test/child.rs");

    /// Captures an expected-failure child with the shared 60-second deadlock watchdog.
    pub(super) fn output(command: &mut Command) -> Output {
        capture(command, CHILD_TIMEOUT).expect("actor panic child must complete")
    }
}

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};

use rca_core::{
    ActionReceipt, ActionTask, DEFAULT_MAILBOX_CAPACITY, DependencyKey, Effect, ReduceContext,
    Reducer, Store,
};

use support::ManualRuntime;

/// Checks an actual returned future's Send bound at compile time.
fn assert_send<T: Send>(_: T) {}

/// Checks a named handle type's Send bound without constructing one.
fn assert_send_type<T: Send>() {}

/// Checks that shared references to the named handle can cross threads.
fn assert_sync_type<T: Sync>() {}

/// Checks both unwind-safety bounds on a named handle type.
fn assert_unwind_safe_type<T: std::panic::UnwindSafe + std::panic::RefUnwindSafe>() {}

/// Checks both unwind-safety bounds on an actual returned future.
fn assert_unwind_safe<T: std::panic::UnwindSafe + std::panic::RefUnwindSafe>(_: T) {}

/// Records a real wake independently of manually polling the associated future.
struct WakeFlag(AtomicBool);

impl std::task::Wake for WakeFlag {
    fn wake(self: Arc<Self>) {
        self.0.store(true, Ordering::Release);
    }
}

/// Gives admission and shutdown scenarios a visible, effect-free state change.
struct ByteCounter;

// Catches listener retirement unwinding after admission but before the actor wake.
#[test]
fn admitted_send_wakes_actor_before_panicking_waiter_cleanup() {
    use std::future::Future;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::atomic::AtomicUsize;
    use std::task::{Wake, Waker};

    struct PanicWake(Arc<AtomicUsize>);
    impl Wake for PanicWake {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
            panic!("neighbouring sender wake panic");
        }
    }

    let runtime = ManualRuntime::new();
    let committed = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&committed);
    let reducer = rca_core::Reduce::new(move |state: &mut usize, action: &usize, _| {
        *state += *action;
        observed.store(*state, Ordering::SeqCst);
        Effect::none()
    });
    let store = Store::builder(0, reducer, runtime.runtime())
        .mailbox_capacity(1)
        .build()
        .expect("valid bounded store");
    let mut actor = runtime.take_only_queued_task();
    let first = store.try_send(1).expect("initial command fits");
    let mut second = Box::pin(store.send(2));
    let mut third = Box::pin(store.send(4));
    let actor_wake = Arc::new(WakeFlag(AtomicBool::new(false)));
    let actor_waker = Waker::from(Arc::clone(&actor_wake));
    let panic_count = Arc::new(AtomicUsize::new(0));
    let neighbour_waker = Waker::from(Arc::new(PanicWake(Arc::clone(&panic_count))));
    let mut cx = Context::from_waker(Waker::noop());
    assert!(second.as_mut().poll(&mut cx).is_pending());
    assert!(
        third
            .as_mut()
            .poll(&mut Context::from_waker(&neighbour_waker))
            .is_pending()
    );
    assert!(
        actor
            .as_mut()
            .poll(&mut Context::from_waker(&actor_waker))
            .is_pending()
    );
    assert_eq!(committed.load(Ordering::SeqCst), 1);
    assert_eq!(panic_count.load(Ordering::SeqCst), 0);
    actor_wake.0.store(false, Ordering::Release);

    let panic = catch_unwind(AssertUnwindSafe(|| second.as_mut().poll(&mut cx)))
        .expect_err("retiring the notified listener wakes its panicking neighbour");
    assert_eq!(
        panic.downcast_ref::<&str>(),
        Some(&"neighbouring sender wake panic")
    );
    assert_eq!(panic_count.load(Ordering::SeqCst), 1);
    let admission_woke_actor = actor_wake.0.load(Ordering::Acquire);
    let full = store
        .try_send(8)
        .expect_err("admitted second command occupies capacity");
    assert!(full.is_full());
    assert_eq!(full.into_action(), 8);
    drop(second);
    drop(third);
    assert!(
        actor
            .as_mut()
            .poll(&mut Context::from_waker(&actor_waker))
            .is_pending()
    );
    let after_poll = committed.load(Ordering::SeqCst);
    drop(first);
    drop(store);
    assert!(
        actor
            .as_mut()
            .poll(&mut Context::from_waker(&actor_waker))
            .is_ready()
    );
    assert!(
        admission_woke_actor,
        "admitted command lost its actor control wake"
    );
    assert_eq!(
        after_poll, 3,
        "surviving admitted action commits after unwind"
    );
}

// A runtime may synchronously submit work from its wake callback. The callback
// must not run under a notification lock that the nested submission acquires.
#[test]
#[cfg_attr(
    miri,
    ignore = "deadlock verification launches a subprocess, which Miri isolation forbids"
)]
fn actor_waker_can_submit_another_action() {
    run_waker_reentry_check(
        "actor_waker_can_submit_another_action",
        check_actor_waker_submission,
    );
}

#[test]
#[cfg_attr(
    miri,
    ignore = "deadlock verification launches a subprocess, which Miri isolation forbids"
)]
fn replaced_actor_waker_can_submit_on_drop() {
    run_waker_reentry_check(
        "replaced_actor_waker_can_submit_on_drop",
        check_replaced_actor_waker_submission,
    );
}

/// Bounds possible callback deadlocks and requires one actual child test to pass.
fn run_waker_reentry_check(test_name: &str, check: fn()) {
    if subprocess::run_in_child(test_name) {
        return;
    }
    check();
}

/// Submits again synchronously from the actor's wake callback.
fn check_actor_waker_submission() {
    use std::sync::atomic::AtomicUsize;
    use std::task::{Wake, Waker};

    /// Allows one nested admission without an unbounded wake recursion.
    struct SubmitOnWake {
        store: Store<usize, u8>,
        sent: AtomicBool,
    }

    impl Wake for SubmitOnWake {
        fn wake(self: Arc<Self>) {
            if !self.sent.swap(true, Ordering::SeqCst) {
                drop(self.store.try_send(2).expect("nested action fits"));
            }
        }
    }

    let runtime = ManualRuntime::new();
    let committed = Arc::new(AtomicUsize::new(0));
    let captured = Arc::clone(&committed);
    let reducer = rca_core::Reduce::new(move |state: &mut usize, action: &u8, _| {
        *state += usize::from(*action);
        captured.store(*state, Ordering::SeqCst);
        Effect::none()
    });
    let store = Store::new(0, reducer, runtime.runtime());
    let mut actor = runtime.take_only_queued_task();
    let wake = Arc::new(SubmitOnWake {
        store: store.clone(),
        sent: AtomicBool::new(false),
    });
    let waker = Waker::from(Arc::clone(&wake));
    assert!(
        actor
            .as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_pending()
    );

    let receipt = store.try_send(1).expect("outer action fits");
    assert!(wake.sent.load(Ordering::SeqCst));
    assert!(
        actor
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    assert_eq!(committed.load(Ordering::SeqCst), 3);
    drop(receipt);
    drop(actor);
}

/// Submits while the actor releases its previous waker, outside notification locks.
fn check_replaced_actor_waker_submission() {
    use std::sync::atomic::AtomicUsize;
    use std::task::{Wake, Waker};

    /// Makes final waker release observable as a real bounded admission.
    struct SubmitOnDrop {
        store: Store<usize, u8>,
        dropped: Arc<AtomicBool>,
    }

    #[expect(
        clippy::manual_noop_waker,
        reason = "The destructor submits work when the actor replaces its waker; Waker::noop would erase that regression."
    )]
    impl Wake for SubmitOnDrop {
        fn wake(self: Arc<Self>) {}
    }

    impl Drop for SubmitOnDrop {
        fn drop(&mut self) {
            drop(
                self.store
                    .try_send(2)
                    .expect("action from waker destructor fits"),
            );
            self.dropped.store(true, Ordering::SeqCst);
        }
    }

    let runtime = ManualRuntime::new();
    let committed = Arc::new(AtomicUsize::new(0));
    let captured = Arc::clone(&committed);
    let reducer = rca_core::Reduce::new(move |state: &mut usize, action: &u8, _| {
        *state += usize::from(*action);
        captured.store(*state, Ordering::SeqCst);
        Effect::none()
    });
    let store = Store::new(0, reducer, runtime.runtime());
    let mut actor = runtime.take_only_queued_task();
    let dropped = Arc::new(AtomicBool::new(false));
    let waker = Waker::from(Arc::new(SubmitOnDrop {
        store: store.clone(),
        dropped: Arc::clone(&dropped),
    }));
    assert!(
        actor
            .as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_pending()
    );
    drop(waker);
    assert!(!dropped.load(Ordering::SeqCst));

    // Replacing the parked actor's waker releases its final owned reference.
    // That safe destructor may submit work just like a wake callback may.
    assert!(
        actor
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    assert!(dropped.load(Ordering::SeqCst));
    assert!(
        actor
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    assert_eq!(committed.load(Ordering::SeqCst), 2);
    drop(actor);
}

/// Exposes duplicate in-future state storage without heap indirection.
struct InlineState<const N: usize>([u8; N]);

/// Keeps the inline state observable without adding another owned copy.
struct InlineReducer<const N: usize>;

impl<const N: usize> Reducer for InlineReducer<N> {
    type State = InlineState<N>;
    type Action = ();

    fn reduce(
        &mut self,
        state: &mut Self::State,
        (): &(),
        _context: &mut ReduceContext<'_>,
    ) -> Effect<()> {
        std::hint::black_box(&state.0);
        Effect::none()
    }
}

#[test]
fn actor_future_reserves_inline_state_only_once() {
    /// Measures the captured actor future before it has ever been polled.
    fn frame_size<const N: usize>() -> usize {
        let runtime = ManualRuntime::new();
        let store = Store::new(InlineState([0; N]), InlineReducer, runtime.runtime());
        let task = runtime.take_only_queued_task();
        let size = std::mem::size_of_val(task.as_ref().get_ref());
        drop(store);
        drop(task);
        size
    }

    let small = frame_size::<0>();
    let large = frame_size::<8192>();
    // Permit alignment/layout slack without freezing any platform's frame size.
    assert!(
        large - small <= 8192 + 64,
        "inline state grew by 8192 bytes but actor future grew from {small} to {large}"
    );
}

#[test]
fn completed_actor_drops_reducer_before_future_is_dropped() {
    /// Records reducer disposal separately from dropping the actor future shell.
    struct DropReducer(Arc<AtomicBool>);
    impl Drop for DropReducer {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }
    impl Reducer for DropReducer {
        type State = ();
        type Action = ();

        fn reduce(&mut self, (): &mut (), (): &(), _: &mut ReduceContext<'_>) -> Effect<()> {
            Effect::none()
        }
    }

    let dropped = Arc::new(AtomicBool::new(false));
    let runtime = ManualRuntime::new();
    let store = Store::new((), DropReducer(Arc::clone(&dropped)), runtime.runtime());
    let mut task = runtime.take_only_queued_task();
    assert!(!dropped.load(Ordering::Acquire));
    drop(store);
    let mut context = Context::from_waker(std::task::Waker::noop());
    assert!(task.as_mut().poll(&mut context).is_ready());
    assert!(dropped.load(Ordering::Acquire));
    drop(task);
}

impl Reducer for ByteCounter {
    type State = usize;
    type Action = u8;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        *state += usize::from(*action);
        Effect::none()
    }
}

/// Injects the specific worker panic checked by the terminal lifecycle scenario.
struct PanicReducer;

impl Reducer for PanicReducer {
    type State = ();
    type Action = ();

    fn reduce(
        &mut self,
        _state: &mut Self::State,
        _action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        panic!("intentional reducer panic")
    }
}

/// Gives the builder override a value distinguishable from its live default.
struct IncrementKey;

impl DependencyKey for IncrementKey {
    type Value = usize;

    fn live_value() -> Self::Value {
        1
    }
}

/// Reads the Store-owned dependency value during real reduction.
struct DependencyCounter;

impl Reducer for DependencyCounter {
    type State = usize;
    type Action = ();

    fn reduce(
        &mut self,
        state: &mut Self::State,
        _action: &Self::Action,
        context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        *state += *context.dependency::<IncrementKey>();
        Effect::none()
    }
}

/// Lets shutdown and failure tests observe state disposal before acknowledgement.
struct DropState(Arc<AtomicBool>);

impl Drop for DropState {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

/// Retains drop-observable state without producing work or changing it.
struct NoOp;

impl Reducer for NoOp {
    type State = DropState;
    type Action = ();

    fn reduce(
        &mut self,
        _state: &mut Self::State,
        _action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        Effect::none()
    }
}

#[test]
fn zero_capacity_fails_before_actor_spawn() {
    let runtime = ManualRuntime::new();
    let dropped = Arc::new(AtomicBool::new(false));
    let builder = Store::builder(DropState(Arc::clone(&dropped)), NoOp, runtime.runtime())
        .mailbox_capacity(1)
        .mailbox_capacity(0);
    assert!(!dropped.load(Ordering::Acquire));
    let error = builder.build().expect_err("zero capacity must fail");

    assert!(error.is_zero_capacity());
    assert!(dropped.load(Ordering::Acquire));
    let display = error.to_string();
    assert!(display.starts_with("store mailbox capacity must exceed zero\n"));
    assert!(display.ends_with(&error.backtrace().to_string()));
    assert!(format!("{error:?}").contains("kind: ZeroCapacity"));
    assert!(std::error::Error::source(&error).is_none());
    assert_eq!(runtime.spawn_count(), 0);
}

#[test]
fn default_mailbox_capacity_is_256() {
    assert_eq!(DEFAULT_MAILBOX_CAPACITY, 256);
    let runtime = ManualRuntime::new();
    let stores = [
        Store::new(0, ByteCounter, runtime.runtime()),
        Store::builder(0, ByteCounter, runtime.runtime())
            .build()
            .expect("default builder is valid"),
    ];
    assert_eq!(runtime.spawn_count(), 2);
    for store in stores {
        for _ in 0..256 {
            drop(store.try_send(1).expect("each default slot is usable"));
        }
        let error = store.try_send(7).expect_err("default mailbox is bounded");
        assert!(error.is_full());
        assert_eq!(error.into_action(), 7);
    }
}

#[test]
fn store_handles_and_futures_cross_threads_safely() {
    assert_send_type::<Store<usize, u8>>();
    assert_sync_type::<Store<usize, u8>>();
    assert_send_type::<ActionReceipt>();
    assert_sync_type::<ActionReceipt>();
    assert_unwind_safe_type::<ActionReceipt>();
    assert_send_type::<ActionTask>();
    assert_sync_type::<ActionTask>();

    let runtime = ManualRuntime::new();
    let store = Store::new(0, ByteCounter, runtime.runtime());
    assert_send(store.send(1));
    assert_send(store.read(|state| *state));
    assert_send(store.observe(|state| *state));
    assert_send(store.observe_distinct(|state| *state));
    assert_send(store.shutdown());
    assert_send(store.closed());

    #[cfg(feature = "test-util")]
    assert_send(store.runtime_snapshot());
    #[cfg(feature = "test-util")]
    assert_send(store.runtime_diagnostics());
    #[cfg(feature = "test-util")]
    let gate = rca_core::testing::AssertionGate::<u8>::new();
    #[cfg(feature = "test-util")]
    assert_send(gate.wait_for_pending());

    assert_send(store.try_send(1).expect("mailbox has capacity").finish());
    assert_unwind_safe(store.try_send(1).expect("mailbox has capacity").finish());
    runtime.start_all();
    let task = futures_executor::block_on(store.send(1)).expect("actor runs");
    assert_send(task.finish());
    let mut observation = futures_executor::block_on(store.observe(|state| *state))
        .expect("actor registers observation");
    assert_send(observation.next());
    futures_executor::block_on(store.shutdown()).expect("actor shuts down");
}

#[test]
fn try_send_retains_action_when_mailbox_is_full() {
    let runtime = ManualRuntime::new();
    let store = Store::builder(0, ByteCounter, runtime.runtime())
        .mailbox_capacity(0)
        .mailbox_capacity(1)
        .build()
        .expect("valid store");

    let first = store.try_send(11_u8).expect("first action fits");
    let error = store.try_send(22_u8).expect_err("second action is full");

    assert!(error.is_full());
    assert_eq!(*error.action(), 22);
    assert_eq!(error.into_action(), 22);
    drop(first);
}

#[test]
fn actor_is_spawned_without_inline_polling_and_commits_actions() {
    let runtime = ManualRuntime::new();
    let store = Store::new(0, ByteCounter, runtime.runtime());
    assert_eq!(runtime.spawn_count(), 1);

    let receipt = store.try_send(7).expect("action fits before actor starts");
    runtime.start_all();
    let task = futures_executor::block_on(async { receipt.await }).expect("action commits");

    assert!(task.is_finished());
    assert!(!task.is_cancelled());
    futures_executor::block_on(task.finish());
}

#[test]
fn cancelling_completed_task_preserves_commit_and_immediate_completion() {
    use futures_util::FutureExt as _;

    let runtime = ManualRuntime::new();
    let store = Store::new(0, ByteCounter, runtime.runtime());
    let mut actor = runtime.take_only_queued_task();
    let mut context = Context::from_waker(std::task::Waker::noop());
    let receipt = store.try_send(7).unwrap();
    assert!(actor.as_mut().poll(&mut context).is_pending());
    let task = receipt.finish().now_or_never().unwrap().unwrap();
    let cloned = task.clone();

    // No effect registered causal work. Cancellation cannot undo this commit.
    task.cancel();
    cloned.cancel();
    for completed in [&task, &cloned] {
        assert!(completed.is_finished());
        assert!(!completed.is_cancelled());
        assert_eq!(completed.finish().now_or_never(), Some(()));
    }

    let mut read = Box::pin(store.read(|state| *state));
    assert!(read.as_mut().poll(&mut context).is_pending());
    assert!(actor.as_mut().poll(&mut context).is_pending());
    assert!(matches!(
        read.as_mut().poll(&mut context),
        Poll::Ready(Ok(7))
    ));
    drop(read);
    drop(store);
    assert!(actor.as_mut().poll(&mut context).is_ready());
}

#[test]
fn fifo_actions_commit_in_admission_order() {
    let runtime = ManualRuntime::new();
    let reducer = rca_core::Reduce::new(|state: &mut Vec<u8>, action: &u8, _| {
        state.push(*action);
        Effect::none()
    });
    let store = Store::new(Vec::new(), reducer, runtime.runtime());
    let first = store.try_send(1).expect("first action fits");
    let second = store.try_send(2).expect("second action fits");
    let third = store.try_send(3).expect("third action fits");
    runtime.start_all();

    futures_executor::block_on(async {
        first.await.expect("first commit").finish().await;
        second.await.expect("second commit").finish().await;
        third.await.expect("third commit").finish().await;
        assert_eq!(
            store.read(Clone::clone).await.expect("ordered read"),
            [1, 2, 3]
        );
    });
}

#[test]
fn cancelling_send_before_admission_submits_nothing() {
    let runtime = ManualRuntime::new();
    let store = Store::builder(0, ByteCounter, runtime.runtime())
        .mailbox_capacity(1)
        .build()
        .expect("valid store");
    let admitted = store.try_send(1).expect("first action fills mailbox");

    let mut pending = Box::pin(store.send(9));
    let waker = futures_util::task::noop_waker();
    let mut context = Context::from_waker(&waker);
    assert!(matches!(
        std::future::Future::poll(pending.as_mut(), &mut context),
        Poll::Pending
    ));
    drop(pending);

    runtime.start_all();
    futures_executor::block_on(async {
        admitted.await.expect("first commit").finish().await;
        assert_eq!(store.read(|state| *state).await.expect("ordered read"), 1);
    });
}

#[test]
fn cancelling_send_after_admission_does_not_retract_action() {
    let runtime = ManualRuntime::new();
    let store = Store::new(0, ByteCounter, runtime.runtime());

    let mut accepted = Box::pin(store.send(4));
    let waker = futures_util::task::noop_waker();
    let mut context = Context::from_waker(&waker);
    assert!(matches!(
        std::future::Future::poll(accepted.as_mut(), &mut context),
        Poll::Pending
    ));
    drop(accepted);

    runtime.start_all();
    assert_eq!(
        futures_executor::block_on(store.read(|state| *state)).expect("ordered read"),
        4
    );
}

#[test]
fn dropping_receipt_does_not_cancel_admitted_action() {
    let runtime = ManualRuntime::new();
    let store = Store::new(0, ByteCounter, runtime.runtime());
    drop(store.try_send(8).expect("action fits"));
    runtime.start_all();

    assert_eq!(
        futures_executor::block_on(store.read(|state| *state)).expect("ordered read"),
        8
    );
}

#[test]
fn pending_receipt_is_woken_when_unpolled_actor_is_dropped() {
    let runtime = ManualRuntime::new();
    let store = Store::new(0, ByteCounter, runtime.runtime());
    let receipt = store.try_send(1).expect("action admitted");
    let mut pending = std::pin::pin!(receipt.finish());
    let flag = Arc::new(WakeFlag(AtomicBool::new(false)));
    let waker = std::task::Waker::from(Arc::clone(&flag));
    let mut context = Context::from_waker(&waker);
    assert!(std::future::Future::poll(pending.as_mut(), &mut context).is_pending());

    assert!(!flag.0.load(Ordering::Acquire));
    runtime.drop_queued();

    assert!(
        flag.0.load(Ordering::Acquire),
        "actor loss must wake the registered waiter"
    );
    assert!(matches!(
        std::future::Future::poll(pending.as_mut(), &mut context),
        Poll::Ready(Err(_))
    ));
}

// Catches a producer wake unwinding before actor teardown drains admitted commands.
#[test]
fn producer_waker_panic_during_actor_drop_rejects_retained_backlog() {
    use std::future::Future as _;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::task::{Wake, Waker};

    struct PanicWake;
    impl Wake for PanicWake {
        fn wake(self: Arc<Self>) {
            panic!("producer wake during actor drop");
        }
    }

    let runtime = ManualRuntime::new();
    let store = Store::builder(
        (),
        rca_core::Reduce::new(|(): &mut (), _: &Arc<()>, _| Effect::none()),
        runtime.runtime(),
    )
    .mailbox_capacity(1)
    .build()
    .unwrap();
    let actor = runtime.take_only_queued_task();
    let payload = Arc::new(());
    let receipt = store.try_send(Arc::clone(&payload)).unwrap();
    let mut receipt = Box::pin(receipt.finish());
    let notified = Arc::new(WakeFlag(AtomicBool::new(false)));
    let receipt_waker = Waker::from(Arc::clone(&notified));
    assert!(
        receipt
            .as_mut()
            .poll(&mut Context::from_waker(&receipt_waker))
            .is_pending()
    );
    let mut blocked = Box::pin(store.send(Arc::new(())));
    let producer_waker = Waker::from(Arc::new(PanicWake));
    assert!(
        blocked
            .as_mut()
            .poll(&mut Context::from_waker(&producer_waker))
            .is_pending()
    );
    assert_eq!(Arc::strong_count(&payload), 2);

    let failure = catch_unwind(AssertUnwindSafe(|| drop(actor)))
        .expect_err("the blocked producer's waker panics during receiver closure");
    // Observe terminal progress before either owner can release retained ingress.
    let receipt_woke = notified.0.load(Ordering::Acquire);
    let terminal = receipt
        .as_mut()
        .poll(&mut Context::from_waker(Waker::noop()));
    let payload_owners = Arc::strong_count(&payload);
    let closed = store.is_closed();

    // Keep the failing witness bounded without letting cleanup satisfy its checks.
    drop(blocked);
    drop(store);

    assert_eq!(
        failure.downcast_ref::<&str>(),
        Some(&"producer wake during actor drop")
    );
    assert!(closed, "actor loss must close Store authority");
    assert!(receipt_woke, "actor loss must wake the admitted receipt");
    assert!(matches!(terminal, Poll::Ready(Err(_))));
    assert_eq!(payload_owners, 1, "actor loss must release queued payloads");
}

// Catches one rejected receipt's wake abandoning later admitted commands.
#[test]
fn receipt_waker_panic_during_actor_drop_rejects_retained_followers() {
    use std::future::Future as _;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::task::{Wake, Waker};

    struct PanicWake;
    impl Wake for PanicWake {
        fn wake(self: Arc<Self>) {
            panic!("receipt wake during actor drop");
        }
    }

    let runtime = ManualRuntime::new();
    let store = Store::builder(
        (),
        rca_core::Reduce::new(|(): &mut (), _: &Arc<()>, _| Effect::none()),
        runtime.runtime(),
    )
    .mailbox_capacity(2)
    .build()
    .unwrap();
    let actor = runtime.take_only_queued_task();
    let first = store.try_send(Arc::new(())).unwrap();
    let payload = Arc::new(());
    let follower = store.try_send(Arc::clone(&payload)).unwrap();
    let mut first = Box::pin(first.finish());
    let mut follower = Box::pin(follower.finish());
    let panic_waker = Waker::from(Arc::new(PanicWake));
    assert!(
        first
            .as_mut()
            .poll(&mut Context::from_waker(&panic_waker))
            .is_pending()
    );
    let notified = Arc::new(WakeFlag(AtomicBool::new(false)));
    let follower_waker = Waker::from(Arc::clone(&notified));
    assert!(
        follower
            .as_mut()
            .poll(&mut Context::from_waker(&follower_waker))
            .is_pending()
    );
    assert_eq!(Arc::strong_count(&payload), 2);

    let failure = catch_unwind(AssertUnwindSafe(|| drop(actor)))
        .expect_err("the first receipt's waker panics during command rejection");
    // The surviving Store must not retain the follower's payload or reply.
    let follower_woke = notified.0.load(Ordering::Acquire);
    let terminal = follower
        .as_mut()
        .poll(&mut Context::from_waker(Waker::noop()));
    let payload_owners = Arc::strong_count(&payload);
    let closed = store.is_closed();

    // Cleanup cannot rescue the terminal observations captured above.
    drop(store);

    assert_eq!(
        failure.downcast_ref::<&str>(),
        Some(&"receipt wake during actor drop")
    );
    assert!(closed, "actor loss must close Store authority");
    assert!(
        follower_woke,
        "actor loss must wake later admitted receipts"
    );
    assert!(matches!(terminal, Poll::Ready(Err(_))));
    assert_eq!(
        payload_owners, 1,
        "actor loss must release queued followers"
    );
}

// Catches unwind cleanup classifying an unentered typed request as normal closure.
#[test]
fn receipt_waker_panic_during_actor_drop_preserves_typed_failure() {
    use std::future::Future as _;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::task::{Wake, Waker};

    use rca_core::{Feature, FeatureContext, Task, TaskAccessReason, TaskContext, TaskProgress};

    struct Root(async_channel::Sender<TaskContext<(), ()>>);
    impl Feature for Root {
        type State = ();
        type Action = ();
        fn update(&mut self, (): &mut (), (): &(), context: &mut FeatureContext<'_, (), ()>) {
            let supply = self.0.clone();
            context.task(Task::run(move |context| async move {
                supply.try_send(context).unwrap();
                std::future::pending::<()>().await;
            }));
        }
    }
    struct PanicWake;
    impl Wake for PanicWake {
        fn wake(self: Arc<Self>) {
            panic!("receipt wake before typed request");
        }
    }

    let runtime = ManualRuntime::new();
    let (supply, supplied) = async_channel::bounded(1);
    let store = Store::builder((), Root(supply), runtime.runtime())
        .mailbox_capacity(2)
        .build()
        .unwrap();
    let mut actor = runtime.take_only_queued_task();
    let start = store.try_send(()).unwrap();
    let mut cx = Context::from_waker(Waker::noop());
    assert!(actor.as_mut().poll(&mut cx).is_pending());
    let mut start = std::pin::pin!(start.finish());
    assert!(matches!(start.as_mut().poll(&mut cx), Poll::Ready(Ok(_))));
    let mut body = runtime.take_only_queued_task();
    assert!(body.as_mut().poll(&mut cx).is_pending());
    let context = supplied.try_recv().unwrap();

    let first = store.try_send(()).unwrap();
    let mut first = Box::pin(first.finish());
    let panic_waker = Waker::from(Arc::new(PanicWake));
    assert!(
        first
            .as_mut()
            .poll(&mut Context::from_waker(&panic_waker))
            .is_pending()
    );
    let selected = Arc::new(AtomicBool::new(false));
    let called = Arc::clone(&selected);
    let payload = Arc::new(());
    let captured = Arc::clone(&payload);
    let mut read = Box::pin(context.read(move |()| {
        drop(captured);
        called.store(true, Ordering::SeqCst);
    }));
    let notified = Arc::new(WakeFlag(AtomicBool::new(false)));
    let read_waker = Waker::from(Arc::clone(&notified));
    assert!(
        read.as_mut()
            .poll(&mut Context::from_waker(&read_waker))
            .is_pending()
    );
    assert!(store.try_send(()).unwrap_err().is_full());
    assert_eq!(Arc::strong_count(&payload), 2);

    let failure = catch_unwind(AssertUnwindSafe(|| drop(actor)))
        .expect_err("the earlier receipt's waker panics before typed rejection");
    // Neither ingress ownership nor the producing task is released before sampling.
    let read_woke = notified.0.load(Ordering::Acquire);
    let terminal = read.as_mut().poll(&mut cx);
    let closed = store.is_closed();
    let payload_owners = Arc::strong_count(&payload);
    drop(body);
    drop(store);

    assert_eq!(
        failure.downcast_ref::<&str>(),
        Some(&"receipt wake before typed request")
    );
    assert!(closed);
    assert!(read_woke, "actor unwind must wake the queued typed request");
    let Poll::Ready(Err(error)) = terminal else {
        panic!("actor unwind must reject the queued typed request immediately");
    };
    assert_eq!(error.reason(), TaskAccessReason::ActorFailed);
    assert_eq!(error.progress(), TaskProgress::NotStarted);
    assert!(!selected.load(Ordering::SeqCst));
    assert_eq!(payload_owners, 1, "unwind must release the uncalled query");
}

#[cfg(feature = "test-util")]
#[test]
fn runtime_inspections_wake_with_store_stopped_when_actor_is_dropped() {
    use futures_util::FutureExt as _;

    let runtime = ManualRuntime::new();
    let dropped = Arc::new(AtomicBool::new(false));
    let store = Store::builder(DropState(Arc::clone(&dropped)), NoOp, runtime.runtime())
        .mailbox_capacity(2)
        .build()
        .expect("valid store");
    let mut snapshot = Box::pin(store.runtime_snapshot());
    let mut diagnostics = Box::pin(store.runtime_diagnostics());
    let snapshot_wake = Arc::new(WakeFlag(AtomicBool::new(false)));
    let diagnostics_wake = Arc::new(WakeFlag(AtomicBool::new(false)));
    let snapshot_waker = std::task::Waker::from(Arc::clone(&snapshot_wake));
    let diagnostics_waker = std::task::Waker::from(Arc::clone(&diagnostics_wake));
    assert!(
        std::future::Future::poll(snapshot.as_mut(), &mut Context::from_waker(&snapshot_waker),)
            .is_pending()
    );
    assert!(
        std::future::Future::poll(
            diagnostics.as_mut(),
            &mut Context::from_waker(&diagnostics_waker),
        )
        .is_pending()
    );
    assert!(
        store
            .try_send(())
            .expect_err("both requests fill the mailbox")
            .is_full()
    );
    assert!(!snapshot_wake.0.load(Ordering::Acquire));
    assert!(!diagnostics_wake.0.load(Ordering::Acquire));
    assert!(!dropped.load(Ordering::Acquire));

    runtime.drop_queued();

    assert!(store.is_closed());
    assert!(dropped.load(Ordering::Acquire));
    assert!(snapshot_wake.0.load(Ordering::Acquire));
    assert!(diagnostics_wake.0.load(Ordering::Acquire));
    let _: rca_core::StoreStopped = snapshot
        .now_or_never()
        .expect("lost actor wakes snapshot")
        .expect_err("lost actor cannot provide a snapshot");
    let _: rca_core::StoreStopped = diagnostics
        .now_or_never()
        .expect("lost actor wakes diagnostics")
        .expect_err("lost actor cannot provide diagnostics");
    let _: rca_core::StoreStopped = store
        .runtime_snapshot()
        .now_or_never()
        .expect("closed ingress fails without actor progress")
        .expect_err("closed store cannot provide a snapshot");
    let _: rca_core::StoreStopped = store
        .runtime_diagnostics()
        .now_or_never()
        .expect("closed ingress fails without actor progress")
        .expect_err("closed store cannot provide diagnostics");
}

#[test]
fn owned_predicate_wait_releases_captures_on_host_loss_before_or_after_admission() {
    for admitted in [false, true] {
        let runtime = ManualRuntime::new();
        let reduced = Arc::new(AtomicBool::new(false));
        let reduction = Arc::clone(&reduced);
        let feature = rca_core::Reduce::new(move |(): &mut (), _: &Arc<()>, _| {
            reduction.store(true, Ordering::Release);
            Effect::none()
        });
        let store = Store::builder((), feature, runtime.runtime())
            .mailbox_capacity(1)
            .build()
            .expect("valid store");
        let action = Arc::new(());
        let action_lifetime = Arc::downgrade(&action);
        let selected = Arc::new(AtomicBool::new(false));
        let selection = Arc::clone(&selected);
        let mut wait = Box::pin(store.send_and_wait_while(action, move |(): &()| {
            selection.store(true, Ordering::Release);
            false
        }));
        let flag = Arc::new(WakeFlag(AtomicBool::new(false)));
        let waker = std::task::Waker::from(Arc::clone(&flag));
        let mut context = Context::from_waker(&waker);
        if admitted {
            assert!(std::future::Future::poll(wait.as_mut(), &mut context).is_pending());
            assert!(
                store
                    .try_send(Arc::new(()))
                    .expect_err("owned action fills the mailbox")
                    .is_full()
            );
            assert!(!flag.0.load(Ordering::Acquire));
        }
        assert!(action_lifetime.upgrade().is_some());
        assert_eq!(Arc::strong_count(&selected), 2);

        runtime.drop_queued();

        assert!(store.is_closed());
        if admitted {
            assert!(flag.0.load(Ordering::Acquire));
            assert!(action_lifetime.upgrade().is_none());
        }
        assert_eq!(
            std::future::Future::poll(wait.as_mut(), &mut context),
            Poll::Ready(rca_core::PredicateWaitOutcome::StoreStopped)
        );
        assert!(!reduced.load(Ordering::Acquire));
        assert!(!selected.load(Ordering::Acquire));
        assert!(action_lifetime.upgrade().is_none());
        assert_eq!(Arc::strong_count(&selected), 1);
    }
}

// Catches an admitted owned wait being reduced after explicit cancellation.
#[test]
fn owned_predicate_wait_cancelled_before_reduction_preserves_open_store() {
    let runtime = ManualRuntime::new();
    let reduced = Arc::new(AtomicBool::new(false));
    let reduction = Arc::clone(&reduced);
    let feature = rca_core::Reduce::new(move |state: &mut u8, _: &Arc<()>, _| {
        reduction.store(true, Ordering::Release);
        *state += 1;
        Effect::none()
    });
    let store = Store::builder(7, feature, runtime.runtime())
        .mailbox_capacity(1)
        .build()
        .expect("valid store");
    let mut actor = runtime.take_only_queued_task();
    let mut context = Context::from_waker(std::task::Waker::noop());
    let action = Arc::new(());
    let action_lifetime = Arc::downgrade(&action);
    let selected = Arc::new(AtomicBool::new(false));
    let selection = Arc::clone(&selected);
    let mut wait = Box::pin(store.send_and_wait_while(action, move |_: &u8| {
        selection.store(true, Ordering::Release);
        false
    }));

    assert!(wait.as_mut().poll(&mut context).is_pending());
    assert!(
        store
            .try_send(Arc::new(()))
            .expect_err("owned action fills the mailbox before the actor is polled")
            .is_full()
    );
    assert!(action_lifetime.upgrade().is_some());
    assert_eq!(Arc::strong_count(&selected), 2);
    assert!(!reduced.load(Ordering::Acquire));
    assert!(!selected.load(Ordering::Acquire));

    wait.cancel();

    assert_eq!(
        wait.as_mut().poll(&mut context),
        Poll::Ready(rca_core::PredicateWaitOutcome::Cancelled)
    );
    assert!(!store.is_closed());
    assert_eq!(Arc::strong_count(&selected), 1);
    assert!(action_lifetime.upgrade().is_some());
    assert!(
        store
            .try_send(Arc::new(()))
            .expect_err("cancellation marks the admitted command without retracting it")
            .is_full()
    );

    assert!(actor.as_mut().poll(&mut context).is_pending());

    assert!(!reduced.load(Ordering::Acquire));
    assert!(!selected.load(Ordering::Acquire));
    assert!(action_lifetime.upgrade().is_none());
    assert!(!store.is_closed());
    assert_eq!(
        wait.as_mut().poll(&mut context),
        Poll::Ready(rca_core::PredicateWaitOutcome::Cancelled)
    );
    assert_eq!(runtime.spawn_count(), 1);
    let mut shutdown = Box::pin(store.shutdown_into_state());
    assert!(shutdown.as_mut().poll(&mut context).is_pending());
    assert!(actor.as_mut().poll(&mut context).is_ready());
    let Poll::Ready(state) = shutdown.as_mut().poll(&mut context) else {
        panic!("completed actor must acknowledge graceful state extraction");
    };
    assert_eq!(state.expect("store stayed open until shutdown"), 7);
    assert_eq!(Arc::strong_count(&reduced), 1);
    assert_eq!(Arc::strong_count(&selected), 1);
}

#[test]
fn builder_dependency_override_is_store_owned() {
    let runtime = ManualRuntime::new();
    let store = Store::builder(0, DependencyCounter, runtime.runtime())
        .with_dependency::<IncrementKey>(9)
        .build()
        .expect("valid store");
    runtime.start_all();

    futures_executor::block_on(async {
        store.send(()).await.expect("commit").finish().await;
        assert_eq!(store.read(|state| *state).await.expect("ordered read"), 9);
    });
}

#[test]
fn final_owner_drop_resolves_non_owning_closed_future() {
    let runtime = ManualRuntime::new();
    let store = Store::new(0, ByteCounter, runtime.runtime());
    let closed = store.closed();
    let clone = store.clone();

    drop(store);
    assert!(!clone.is_closed());
    drop(clone);
    futures_executor::block_on(closed);
}

#[test]
fn read_observes_prior_commit_and_not_later_commit() {
    let runtime = ManualRuntime::new();
    let store = Store::new(0, ByteCounter, runtime.runtime());
    runtime.start_all();

    assert_send(store.read(|state| *state));

    futures_executor::block_on(async {
        store.send(1).await.expect("first commit").finish().await;
        let observed = store.read(|state| *state).await.expect("ordered read");
        store.send(2).await.expect("second commit").finish().await;

        assert_eq!(observed, 1);
        assert_eq!(store.read(|state| *state).await.expect("final read"), 3);
    });
}

#[test]
fn ordered_shutdown_commits_prior_action_and_rejects_later_work() {
    let runtime = ManualRuntime::new();
    let store = Store::new(0, ByteCounter, runtime.runtime());
    runtime.start_all();

    futures_executor::block_on(async {
        store.send(3).await.expect("prior action").finish().await;
        store.shutdown().await.expect("ordered shutdown");

        assert!(store.is_closed());
        store.send(5).await.expect_err("closed Store rejects send");
        let error = store
            .try_send(7)
            .expect_err("closed Store rejects admission");
        assert!(error.is_store_stopped());
        assert_eq!(error.into_action(), 7);
        store.shutdown().await.expect("idempotent shutdown");
    });
}

#[test]
fn shutdown_drops_actor_state_before_acknowledgement() {
    let dropped = Arc::new(AtomicBool::new(false));
    let runtime = ManualRuntime::new();
    let store = Store::new(DropState(Arc::clone(&dropped)), NoOp, runtime.runtime());
    runtime.start_all();

    futures_executor::block_on(store.shutdown()).expect("ordered shutdown");
    assert!(dropped.load(Ordering::Acquire));
}

#[test]
fn backpressured_shutdown_reports_actor_loss_without_acknowledging_queued_work() {
    use futures_util::FutureExt as _;

    let dropped = Arc::new(AtomicBool::new(false));
    let runtime = ManualRuntime::new();
    let store = Store::builder(DropState(Arc::clone(&dropped)), NoOp, runtime.runtime())
        .mailbox_capacity(1)
        .build()
        .unwrap();
    let queued = store.try_send(()).unwrap();
    assert!(store.try_send(()).unwrap_err().is_full());
    let mut shutdown = Box::pin(store.shutdown());
    let wake = Arc::new(WakeFlag(AtomicBool::new(false)));
    let waker = std::task::Waker::from(Arc::clone(&wake));
    let mut context = Context::from_waker(&waker);
    assert!(shutdown.as_mut().poll(&mut context).is_pending());
    assert!(!dropped.load(Ordering::Acquire));
    wake.0.store(false, Ordering::Release);

    runtime.drop_queued();
    assert!(wake.0.load(Ordering::Acquire));
    let error: rca_core::ShutdownError = shutdown
        .now_or_never()
        .expect("actor loss wakes shutdown without further executor progress")
        .unwrap_err();
    assert!(error.is_store_stopped());
    let _: rca_core::StoreStopped = queued
        .finish()
        .now_or_never()
        .expect("actor loss rejects the queued action")
        .unwrap_err();
    assert!(dropped.load(Ordering::Acquire));
    assert!(store.is_closed());
    let repeated: rca_core::ShutdownError = store
        .shutdown()
        .now_or_never()
        .expect("abortive closure rejects a new shutdown without driving the actor")
        .unwrap_err();
    assert!(repeated.is_store_stopped());
}

#[test]
#[cfg_attr(
    miri,
    ignore = "panic verification launches a subprocess, which Miri isolation forbids"
)]
fn reducer_panic_closes_store_without_recovery() {
    const CHILD_ENV: &str = "RCA_REDUCER_PANIC_CHILD";

    if std::env::var_os(CHILD_ENV).is_none() {
        let output = subprocess::output(
            std::process::Command::new(
                std::env::current_exe().expect("test executable path must exist"),
            )
            .args([
                "--exact",
                "reducer_panic_closes_store_without_recovery",
                "--nocapture",
            ])
            .env(CHILD_ENV, "1"),
        );

        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("intentional reducer panic"),
            "child stderr must preserve original reducer panic"
        );
        assert!(
            String::from_utf8_lossy(&output.stdout)
                .contains("pending receipt woke after actor panic")
        );
        return;
    }

    let runtime = ManualRuntime::new();
    let store = Store::new((), PanicReducer, runtime.runtime());
    let closed = store.closed();
    let receipt = store.try_send(()).expect("panic action fits");
    let mut pending = std::pin::pin!(receipt.finish());
    let flag = Arc::new(WakeFlag(AtomicBool::new(false)));
    let waker = std::task::Waker::from(Arc::clone(&flag));
    let mut context = Context::from_waker(&waker);
    assert!(std::future::Future::poll(pending.as_mut(), &mut context).is_pending());
    assert!(!flag.0.load(Ordering::Acquire));
    runtime.start_all();
    let panic = runtime
        .join_all()
        .into_iter()
        .next()
        .expect("actor thread must exist")
        .expect_err("actor thread must propagate reducer panic");
    assert_eq!(
        panic.downcast_ref::<&str>(),
        Some(&"intentional reducer panic")
    );
    assert!(
        flag.0.load(Ordering::Acquire),
        "actor panic must wake the registered waiter"
    );
    assert!(matches!(
        std::future::Future::poll(pending.as_mut(), &mut context),
        Poll::Ready(Err(_))
    ));
    futures_executor::block_on(closed);
    assert!(store.is_closed());
    futures_executor::block_on(store.send(())).expect_err("failed Store rejects send");

    println!("pending receipt woke after actor panic");
    std::panic::resume_unwind(panic);
}

// A restored cold keyed snapshot must not outlive its failed actor.
#[test]
#[cfg_attr(
    miri,
    ignore = "panic verification launches a subprocess, which Miri isolation forbids"
)]
fn keyed_extractor_panic_releases_actor_resources() {
    use rca_core::{KeyedAction, KeyedActionPath, KeyedState, KeyedStatePath, Reduce, for_each};

    type State = KeyedState<Arc<u8>, DropState>;
    type Action = KeyedAction<Arc<u8>, ()>;
    const CHILD_ENV: &str = "RCA_KEYED_ACTOR_PANIC_CHILD";
    const VERIFIED: &str = "keyed extractor actor teardown verified";
    if std::env::var_os(CHILD_ENV).is_none() {
        let output = subprocess::output(
            std::process::Command::new(
                std::env::current_exe().expect("test executable path must exist"),
            )
            .args([
                "--exact",
                "keyed_extractor_panic_releases_actor_resources",
                "--nocapture",
            ])
            .env(CHILD_ENV, "1"),
        );
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stdout).contains(VERIFIED));
        return;
    }

    let payload = Arc::new(());
    let extractor_payload = Arc::clone(&payload);
    let invoked = Arc::new(AtomicBool::new(false));
    let parent_invoked = Arc::clone(&invoked);
    let child_invoked = Arc::clone(&invoked);
    let reducer = for_each(
        Reduce::new(move |_: &mut State, _: &Action, _| {
            parent_invoked.store(true, Ordering::Release);
            Effect::none()
        }),
        KeyedStatePath::new(|state: &State| state, |state: &mut State| state),
        KeyedActionPath::new(
            move |action: &Action| -> Option<&Action> {
                // State and action own two references; cold reconciliation adds one.
                assert!(Arc::strong_count(action.id()) > 2);
                std::panic::panic_any(Arc::clone(&extractor_payload));
            },
            |action| action,
        ),
        Reduce::new(move |_: &mut DropState, (): &(), _| {
            child_invoked.store(true, Ordering::Release);
            Effect::none()
        }),
    );
    let dropped = Arc::new(AtomicBool::new(false));
    let id = Arc::new(7_u8);
    let id_lifetime = Arc::downgrade(&id);
    let state: State = [(Arc::clone(&id), DropState(Arc::clone(&dropped)))]
        .into_iter()
        .collect();
    let runtime = ManualRuntime::new();
    let store = Store::new(state, reducer, runtime.runtime());
    let mut closed = std::pin::pin!(store.closed());
    let receipt = store
        .try_send(Action::new(id, ()))
        .expect("panic action fits");
    let mut pending = std::pin::pin!(receipt.finish());
    let wake = Arc::new(WakeFlag(AtomicBool::new(false)));
    let waker = std::task::Waker::from(Arc::clone(&wake));
    let mut context = Context::from_waker(&waker);
    assert!(pending.as_mut().poll(&mut context).is_pending());
    assert!(!wake.0.load(Ordering::Acquire));
    assert!(!dropped.load(Ordering::Acquire));
    runtime.start_all();
    let panic = runtime
        .join_all()
        .into_iter()
        .next()
        .expect("actor thread must exist")
        .expect_err("actor must propagate the extractor panic");
    assert!(Arc::ptr_eq(
        panic.downcast_ref::<Arc<()>>().unwrap(),
        &payload
    ));
    assert!(wake.0.load(Ordering::Acquire));
    let Poll::Ready(result) = pending.as_mut().poll(&mut context) else {
        panic!("actor panic must wake the pending receipt");
    };
    let _: rca_core::StoreStopped = result.expect_err("failed action must not commit");
    assert!(!invoked.load(Ordering::Acquire));
    assert!(dropped.load(Ordering::Acquire));
    assert!(
        id_lifetime.upgrade().is_none(),
        "actor must release cached IDs"
    );
    assert!(closed.as_mut().poll(&mut context).is_ready());
    assert!(store.is_closed());
    assert!(
        store
            .try_send(Action::new(Arc::new(9), ()))
            .expect_err("failed actor must close ingress")
            .is_store_stopped()
    );
    println!("{VERIFIED}");
    std::panic::resume_unwind(panic);
}

#[test]
fn host_runtime_teardown_closes_unpolled_actor() {
    let runtime = ManualRuntime::new();
    let store = Store::new(0, ByteCounter, runtime.runtime());
    let closed = store.closed();

    drop(runtime);

    assert!(store.is_closed());
    futures_executor::block_on(closed);
}

#[cfg(feature = "test-util")]
#[test]
#[expect(
    clippy::too_many_lines,
    reason = "manual polls keep each gate transition and negative assertion bounded"
)]
fn latest_snapshot_reads_do_not_drive_assertions_or_release_effect_actions() {
    use std::sync::atomic::AtomicUsize;

    use futures_util::FutureExt as _;
    use rca_core::testing::{AssertionGate, RuntimeSnapshot};

    struct GatedCounter;
    impl Reducer for GatedCounter {
        type State = usize;
        type Action = u8;

        fn reduce(
            &mut self,
            state: &mut usize,
            action: &u8,
            _: &mut ReduceContext<'_>,
        ) -> Effect<u8> {
            *state += usize::from(*action);
            if *action == 1 {
                Effect::send(2)
            } else {
                Effect::none()
            }
        }
    }

    let runtime = ManualRuntime::new();
    let gate = AssertionGate::default();
    let store = Store::builder(0, GatedCounter, runtime.runtime())
        .assertion_gate(gate.clone())
        .build()
        .expect("valid store");
    let mut actor = runtime.take_only_queued_task();
    let mut context = Context::from_waker(std::task::Waker::noop());
    assert_eq!(gate.latest_snapshot(), None);

    let first = store.try_send(4).expect("ordinary action fits");
    assert!(actor.as_mut().poll(&mut context).is_pending());
    assert!(
        first
            .finish()
            .now_or_never()
            .expect("first receipt ready")
            .expect("first commit")
            .is_finished()
    );
    let settled = RuntimeSnapshot {
        commit_version: 1,
        live_task_ids: vec![],
        cancellation_count: 0,
        timer_deadlines: vec![],
        observation_count: 0,
    };
    assert_eq!(gate.latest_snapshot(), Some(settled.clone()));

    let start = store.try_send(1).expect("effect-producing action fits");
    assert!(actor.as_mut().poll(&mut context).is_pending());
    let task = start
        .finish()
        .now_or_never()
        .expect("start receipt ready")
        .expect("start committed");
    let mut effect = runtime.take_only_queued_task();
    assert!(effect.as_mut().poll(&mut context).is_pending());
    assert!(actor.as_mut().poll(&mut context).is_pending());
    assert_eq!(gate.inspect_pending(|action| *action), Some(2));
    let pending = gate
        .pending_snapshot()
        .expect("effect paused before commit");
    assert_eq!(pending.commit_version, 2);
    assert_eq!(pending.live_task_ids.len(), 1);
    assert_eq!(pending.cancellation_count, 0);
    assert_eq!(
        pending.timer_deadlines,
        [] as [rca_core::MonotonicInstant; 0]
    );
    assert_eq!(pending.observation_count, 0);

    let calls = Arc::new(AtomicUsize::new(0));
    let assertion_calls = Arc::clone(&calls);
    let assertion = gate.assert_state(move |state: &usize| {
        assert_eq!(*state, 5);
        assertion_calls.fetch_add(1, Ordering::SeqCst);
    });
    assert_eq!(format!("{assertion:?}"), "AssertionStateReceipt { .. }");
    let mut assertion = Box::pin(assertion.finish());
    for _ in 0..3 {
        assert_eq!(gate.latest_snapshot(), Some(pending.clone()));
        assert_eq!(
            gate.latest_diagnostics()
                .expect("diagnostics retained")
                .runtime(),
            &pending
        );
        assert!(gate.has_pending());
        assert!(!task.is_finished());
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(std::future::Future::poll(assertion.as_mut(), &mut context).is_pending());
    }

    assert!(actor.as_mut().poll(&mut context).is_pending());
    assert!(matches!(
        std::future::Future::poll(assertion.as_mut(), &mut context),
        Poll::Ready(Ok(()))
    ));
    for _ in 0..3 {
        assert_eq!(gate.latest_snapshot(), Some(pending.clone()));
        assert!(actor.as_mut().poll(&mut context).is_pending());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(gate.has_pending());
        assert!(!task.is_finished());
    }

    let release = gate
        .release_pending_with_receipt()
        .expect("explicit release");
    assert_eq!(format!("{release:?}"), "AssertionReceipt { .. }");
    assert!(!task.is_finished());
    assert!(actor.as_mut().poll(&mut context).is_pending());
    let released = release
        .finish()
        .now_or_never()
        .expect("release receipt ready")
        .expect("effect committed");
    assert!(effect.as_mut().poll(&mut context).is_ready());
    assert!(actor.as_mut().poll(&mut context).is_pending());
    assert!(task.is_finished());
    assert!(released.is_finished());
    assert!(!gate.has_pending());
    for _ in 0..3 {
        assert_eq!(
            gate.latest_snapshot(),
            Some(RuntimeSnapshot {
                commit_version: 3,
                ..settled.clone()
            })
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
    let mut read = Box::pin(store.read(|state| *state));
    assert!(std::future::Future::poll(read.as_mut(), &mut context).is_pending());
    assert!(actor.as_mut().poll(&mut context).is_pending());
    assert!(matches!(
        std::future::Future::poll(read.as_mut(), &mut context),
        Poll::Ready(Ok(7))
    ));
    drop(read);
    drop(store);
    assert!(actor.as_mut().poll(&mut context).is_ready());
}

#[cfg(feature = "test-util")]
#[test]
fn dropped_state_assertion_receipt_skips_callback_without_releasing_action() {
    use std::sync::atomic::AtomicUsize;

    use futures_util::FutureExt as _;
    use rca_core::testing::AssertionGate;

    /// Counts disposal independently of whether the assertion executes.
    struct CaptureDrop(Arc<AtomicUsize>);

    impl Drop for CaptureDrop {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    let runtime = ManualRuntime::new();
    let gate = AssertionGate::new();
    let reducer = rca_core::Reduce::new(|state: &mut usize, action: &u8, _| {
        *state += usize::from(*action);
        if *action == 1 {
            Effect::send(2)
        } else {
            Effect::none()
        }
    });
    let store = Store::builder(0, reducer, runtime.runtime())
        .assertion_gate(gate.clone())
        .build()
        .unwrap();
    let mut actor = runtime.take_only_queued_task();
    let mut context = Context::from_waker(std::task::Waker::noop());
    let start = store.try_send(1).unwrap();
    assert!(actor.as_mut().poll(&mut context).is_pending());
    let task = start.finish().now_or_never().unwrap().unwrap();
    let mut effect = runtime.take_only_queued_task();
    assert!(effect.as_mut().poll(&mut context).is_pending());
    assert!(actor.as_mut().poll(&mut context).is_pending());
    assert_eq!(gate.inspect_pending(|action| *action), Some(2));

    let drops = Arc::new(AtomicUsize::new(0));
    let capture = CaptureDrop(Arc::clone(&drops));
    let abandoned_calls = Arc::new(AtomicUsize::new(0));
    let called = Arc::clone(&abandoned_calls);
    let abandoned = gate.assert_state(move |_: &usize| {
        called.fetch_add(1, Ordering::SeqCst);
        drop(capture);
    });
    drop(abandoned);
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    let retained_calls = Arc::new(AtomicUsize::new(0));
    let called = Arc::clone(&retained_calls);
    let retained = gate.assert_state(move |state: &usize| {
        assert_eq!(*state, 1);
        called.fetch_add(1, Ordering::SeqCst);
    });

    assert!(actor.as_mut().poll(&mut context).is_pending());
    retained.finish().now_or_never().unwrap().unwrap();
    assert_eq!(abandoned_calls.load(Ordering::SeqCst), 0);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert_eq!(retained_calls.load(Ordering::SeqCst), 1);
    assert_eq!(gate.inspect_pending(|action| *action), Some(2));
    assert!(!task.is_finished());

    let release = gate.release_pending_with_receipt().unwrap();
    assert!(actor.as_mut().poll(&mut context).is_pending());
    let released = release.finish().now_or_never().unwrap().unwrap();
    assert!(effect.as_mut().poll(&mut context).is_ready());
    // Runtime completion leaves causal work registered until the actor harvests it.
    assert!(!task.is_finished());
    assert!(actor.as_mut().poll(&mut context).is_pending());
    assert!(task.is_finished());
    assert!(released.is_finished());
    assert!(!gate.has_pending());
    let mut read = Box::pin(store.read(|state| *state));
    assert!(read.as_mut().poll(&mut context).is_pending());
    assert!(actor.as_mut().poll(&mut context).is_pending());
    assert!(matches!(
        read.as_mut().poll(&mut context),
        Poll::Ready(Ok(3))
    ));
    drop(read);
    drop(store);
    assert!(actor.as_mut().poll(&mut context).is_ready());
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert_eq!(retained_calls.load(Ordering::SeqCst), 1);
}

#[cfg(feature = "test-util")]
#[test]
fn final_owner_close_during_pending_inspection_does_not_restore_command() {
    use std::sync::atomic::AtomicUsize;

    use futures_util::FutureExt as _;
    use rca_core::testing::AssertionGate;

    let runtime = ManualRuntime::new();
    let gate = AssertionGate::new();
    let reductions = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&reductions);
    let reducer = rca_core::Reduce::new(move |state: &mut u8, action: &(u8, Arc<()>), _| {
        counted.fetch_add(1, Ordering::SeqCst);
        *state += action.0;
        if action.0 == 1 {
            Effect::send((2, Arc::clone(&action.1)))
        } else {
            Effect::none()
        }
    });
    let store = Store::builder(0, reducer, runtime.runtime())
        .assertion_gate(gate.clone())
        .build()
        .unwrap();
    let payload = Arc::new(());
    let started = store.try_send((1, Arc::clone(&payload))).unwrap();
    let mut actor = runtime.take_only_queued_task();
    let mut context = Context::from_waker(std::task::Waker::noop());
    assert!(actor.as_mut().poll(&mut context).is_pending());
    let task = started.finish().now_or_never().unwrap().unwrap();
    let mut effect = runtime.take_only_queued_task();
    assert!(effect.as_mut().poll(&mut context).is_pending());
    assert!(actor.as_mut().poll(&mut context).is_pending());
    assert!(gate.has_pending());
    assert!(!task.is_finished());
    assert_eq!(Arc::strong_count(&payload), 2);
    let closed = store.closed();

    gate.inspect_pending(|action| {
        assert_eq!(action.0, 2);
        assert!(Arc::ptr_eq(&action.1, &payload));
        let release = gate.release_pending_with_receipt().unwrap();
        drop(store);
        // The real actor must finish while the callback still owns the command.
        assert!(actor.as_mut().poll(&mut context).is_ready());
        assert!(closed.now_or_never().is_some());
        assert!(!gate.has_pending());
        assert!(gate.pending_snapshot().is_none());
        release
            .finish()
            .now_or_never()
            .expect("actor closure resolves queued assertion receipt")
            .expect_err("owner loss prevents release");
        assert_eq!(Arc::strong_count(&payload), 2);
    })
    .expect("real pending effect is inspected");

    assert!(!gate.has_pending());
    assert!(!gate.release_pending());
    assert_eq!(Arc::strong_count(&payload), 1);
    assert_eq!(reductions.load(Ordering::SeqCst), 1);
    assert!(task.is_cancelled());
    assert!(effect.as_mut().poll(&mut context).is_ready());
    assert!(task.finish().now_or_never().is_some());
    assert_eq!(runtime.spawn_count(), 2);
}

#[cfg(feature = "test-util")]
#[test]
fn state_assertion_authority_change_is_rechecked_before_gate_wait() {
    use std::sync::atomic::AtomicUsize;

    use futures_util::FutureExt as _;
    use rca_core::testing::AssertionGate;

    for close in [false, true] {
        let runtime = ManualRuntime::new();
        let gate = AssertionGate::new();
        let reductions = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&reductions);
        let reducer = rca_core::Reduce::new(move |state: &mut u8, action: &(u8, Arc<()>), _| {
            counted.fetch_add(1, Ordering::SeqCst);
            *state += action.0;
            if action.0 == 1 {
                Effect::send((2, Arc::clone(&action.1)))
            } else {
                Effect::none()
            }
        });
        let store = Store::builder(0, reducer, runtime.runtime())
            .assertion_gate(gate.clone())
            .build()
            .unwrap();
        let payload = Arc::new(());
        let started = store.try_send((1, Arc::clone(&payload))).unwrap();
        let mut actor = runtime.take_only_queued_task();
        let mut context = Context::from_waker(std::task::Waker::noop());
        assert!(actor.as_mut().poll(&mut context).is_pending());
        let task = started.finish().now_or_never().unwrap().unwrap();
        let mut effect = runtime.take_only_queued_task();
        assert!(effect.as_mut().poll(&mut context).is_pending());
        assert!(actor.as_mut().poll(&mut context).is_pending());
        assert_eq!(gate.inspect_pending(|action| action.0), Some(2));
        assert_eq!(Arc::strong_count(&payload), 2);
        assert!(!task.is_finished());
        let closed = store.closed();
        let mut retained_store = Some(store);
        let last_owner = close.then(|| retained_store.take().unwrap());
        let cancel = (!close).then(|| task.clone());
        let assertion = gate.assert_state(move |state: &u8| {
            assert_eq!(*state, 1);
            if let Some(task) = cancel {
                task.cancel();
            }
            drop(last_owner);
        });

        // No release/skip decision: the callback changes authority after the
        // first liveness check, before the actor registers its next listeners.
        assert_eq!(actor.as_mut().poll(&mut context).is_ready(), close);
        assertion
            .finish()
            .now_or_never()
            .expect("state callback completes in the actor poll")
            .unwrap();
        assert!(!gate.has_pending());
        assert!(gate.pending_snapshot().is_none());
        assert_eq!(Arc::strong_count(&payload), 1);
        assert_eq!(reductions.load(Ordering::SeqCst), 1);
        assert!(task.is_cancelled());
        assert!(effect.as_mut().poll(&mut context).is_ready());

        if let Some(store) = retained_store {
            assert!(!store.is_closed());
            let mut read = Box::pin(store.read(|state| *state));
            assert!(read.as_mut().poll(&mut context).is_pending());
            assert!(actor.as_mut().poll(&mut context).is_pending());
            assert!(matches!(
                read.as_mut().poll(&mut context),
                Poll::Ready(Ok(1))
            ));
            drop(read);
            assert!(task.finish().now_or_never().is_some());
            drop(store);
            assert!(actor.as_mut().poll(&mut context).is_ready());
        }
        assert!(closed.now_or_never().is_some());
        assert!(task.finish().now_or_never().is_some());
        assert_eq!(runtime.spawn_count(), 2);
    }
}

#[test]
#[ignore = "ten-second idle CPU profiling probe"]
fn idle_actor_profile_window() {
    let runtime = ManualRuntime::new();
    let store = Store::new(0, ByteCounter, runtime.runtime());
    runtime.start_all();

    std::thread::sleep(std::time::Duration::from_secs(10));

    assert_eq!(
        futures_executor::block_on(store.read(|state| *state)).expect("idle read"),
        0
    );
    futures_executor::block_on(store.shutdown()).expect("clean shutdown");
}
