//! Checks payload ownership, FIFO admission and cleanup before shutdown acknowledgement.

use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Wake, Waker};

use rca_core::{
    ActionPath, ActionReceipt, Effect, OptionalStatePath, ReduceContext, Reducer, ScopedStore,
    StatePath, Store,
};
use rca_test::DeterministicRuntime;

/// Gives each queued allocation an identity and a separate drop count.
#[derive(Debug)]
struct Payload {
    token: u8,
    drops: Arc<AtomicUsize>,
}

impl Drop for Payload {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

/// Deliberately lacks `Clone` and `Debug`: errors must return the same allocation
/// and produce diagnostics without inspecting the action payload.
struct OwnedAction(Box<Payload>);

/// Creates a distinct allocation whose release remains visible after rejection.
fn owned(token: u8, drops: &Arc<AtomicUsize>) -> OwnedAction {
    OwnedAction(Box::new(Payload {
        token,
        drops: Arc::clone(drops),
    }))
}

/// Separates owned work from the action that ends an optional child scope.
enum Action {
    Owned(OwnedAction),
    RemoveChild,
}

/// Holds required and optional children and records release of the root state.
struct State {
    child: (),
    optional_child: Option<()>,
    drops: Arc<AtomicUsize>,
}

impl Drop for State {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

/// Records reduction order and release independently of the action payloads.
struct Feature {
    reductions: Arc<Mutex<Vec<u8>>>,
    drops: Arc<AtomicUsize>,
}

impl Reducer for Feature {
    type State = State;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut State,
        action: &Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        match action {
            Action::Owned(action) => self.reductions.lock().unwrap().push(action.0.token),
            Action::RemoveChild => {
                state.optional_child = None;
                self.reductions.lock().unwrap().push(0);
            }
        }
        Effect::none()
    }
}

impl Drop for Feature {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

/// Keeps the host and independent ownership counters beside the real store.
struct Fixture {
    host: DeterministicRuntime,
    store: Store<State, Action>,
    reductions: Arc<Mutex<Vec<u8>>>,
    state_drops: Arc<AtomicUsize>,
    reducer_drops: Arc<AtomicUsize>,
}

impl Fixture {
    /// Builds a store with a fixed capacity so admission can be checked before polling.
    fn new(capacity: usize) -> Self {
        let host = DeterministicRuntime::new();
        let reductions = Arc::new(Mutex::new(Vec::new()));
        let state_drops = Arc::new(AtomicUsize::new(0));
        let reducer_drops = Arc::new(AtomicUsize::new(0));
        let store = Store::builder(
            State {
                child: (),
                optional_child: Some(()),
                drops: Arc::clone(&state_drops),
            },
            Feature {
                reductions: Arc::clone(&reductions),
                drops: Arc::clone(&reducer_drops),
            },
            host.runtime(),
        )
        .mailbox_capacity(capacity)
        .build()
        .expect("positive mailbox capacity");
        Self {
            host,
            store,
            reductions,
            state_drops,
            reducer_drops,
        }
    }

    /// Requires the acknowledgement to follow state, reducer and scheduler cleanup.
    fn shutdown(&self) {
        futures_executor::block_on(self.host.run(self.store.shutdown()))
            .expect("deterministic shutdown drive")
            .expect("ordinary shutdown acknowledgement");
        assert!(self.store.is_closed());
        assert_eq!(self.state_drops.load(Ordering::SeqCst), 1);
        assert_eq!(self.reducer_drops.load(Ordering::SeqCst), 1);
        futures_executor::block_on(self.host.drain_deadlines(0))
            .expect("no scheduler tasks or timers remain");
    }
}

/// Counts embedding separately from reduction to detect work on rejected actions.
fn required_child(
    store: &Store<State, Action>,
    embeds: &Arc<AtomicUsize>,
) -> ScopedStore<(), OwnedAction> {
    let embeds = Arc::clone(embeds);
    store.scope(
        StatePath::new(
            |state: &State| &state.child,
            |state: &mut State| &mut state.child,
        ),
        ActionPath::new(
            |action: &Action| match action {
                Action::Owned(action) => Some(action),
                Action::RemoveChild => None,
            },
            move |action| {
                embeds.fetch_add(1, Ordering::SeqCst);
                Action::Owned(action)
            },
        ),
    )
}

/// Checks both admission completion and completion of its causal task.
fn complete(receipt: ActionReceipt) {
    let task = futures_executor::block_on(receipt.finish()).expect("admitted action committed");
    futures_executor::block_on(task.finish());
    assert!(task.is_finished());
}

/// Checks readiness once without advancing the deterministic host.
fn poll<F: Future>(future: Pin<&mut F>) -> Poll<F::Output> {
    future.poll(&mut Context::from_waker(Waker::noop()))
}

#[test]
fn root_full_returns_the_original_move_only_action_without_reducing_it() {
    // Catches dropped/replaced Full payloads, or reduction after rejected admission.
    let fixture = Fixture::new(1);
    let queued_drops = Arc::new(AtomicUsize::new(0));
    let rejected_drops = Arc::new(AtomicUsize::new(0));
    let queued = fixture
        .store
        .try_send(Action::Owned(owned(11, &queued_drops)))
        .expect("fills the mailbox before host progress");
    let rejected = owned(22, &rejected_drops);
    let pointer = std::ptr::from_ref(rejected.0.as_ref());
    let error = fixture
        .store
        .try_send(Action::Owned(rejected))
        .expect_err("Full");
    assert!(error.is_full());
    assert!(!error.is_store_stopped());
    let Action::Owned(borrowed) = error.action() else {
        panic!("borrowed rejection must retain the original action variant");
    };
    assert_eq!(std::ptr::from_ref(borrowed.0.as_ref()), pointer);
    assert!(format!("{error:?}").starts_with("TrySendError { kind: Full,"));
    assert_eq!(
        error.to_string().lines().next(),
        Some("store ingress is full")
    );
    let backtrace = error.backtrace().to_string();
    assert!(error.to_string().ends_with(&backtrace));
    assert!(std::error::Error::source(&error).is_none());
    assert_eq!(rejected_drops.load(Ordering::SeqCst), 0);
    assert_eq!(queued_drops.load(Ordering::SeqCst), 0);
    assert_eq!(*fixture.reductions.lock().unwrap(), []);
    let Action::Owned(recovered) = error.into_action() else {
        panic!("the original action variant must be recovered");
    };
    assert_eq!(std::ptr::from_ref(recovered.0.as_ref()), pointer);
    assert_eq!(recovered.0.token, 22);
    assert_eq!(rejected_drops.load(Ordering::SeqCst), 0);
    drop(recovered);
    assert_eq!(rejected_drops.load(Ordering::SeqCst), 1);
    fixture
        .host
        .run_until_stalled()
        .expect("queued useful work");
    complete(queued);
    assert_eq!(*fixture.reductions.lock().unwrap(), [11]);
    assert_eq!(queued_drops.load(Ordering::SeqCst), 1);
    fixture.shutdown();
    assert_eq!(rejected_drops.load(Ordering::SeqCst), 1);
}

#[test]
fn root_stopped_returns_the_original_move_only_action_without_reducing_it() {
    // Catches a Closed branch that destroys/replaces the caller's payload.
    let fixture = Fixture::new(1);
    fixture.shutdown();
    let drops = Arc::new(AtomicUsize::new(0));
    let action = owned(7, &drops);
    let pointer = std::ptr::from_ref(action.0.as_ref());
    let error = fixture
        .store
        .try_send(Action::Owned(action))
        .expect_err("stopped root");
    assert!(error.is_store_stopped());
    assert!(!error.is_full());
    let Action::Owned(borrowed) = error.action() else {
        panic!("borrowed rejection must retain the original action variant");
    };
    assert_eq!(std::ptr::from_ref(borrowed.0.as_ref()), pointer);
    assert!(format!("{error:?}").starts_with("TrySendError { kind: StoreStopped,"));
    assert_eq!(
        error.to_string().lines().next(),
        Some("store stopped before action admission")
    );
    assert!(std::error::Error::source(&error).is_none());
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    let Action::Owned(recovered) = error.into_action() else {
        panic!("the original action variant must be recovered");
    };
    assert_eq!(std::ptr::from_ref(recovered.0.as_ref()), pointer);
    assert_eq!(recovered.0.token, 7);
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    drop(recovered);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert_eq!(*fixture.reductions.lock().unwrap(), []);
}

#[test]
fn scoped_full_recovers_move_only_action_without_embedding_or_reduction() {
    // Catches eager embedding before Full admission, or loss of the child payload.
    let fixture = Fixture::new(1);
    let embeds = Arc::new(AtomicUsize::new(0));
    let child = required_child(&fixture.store, &embeds);
    let queued_drops = Arc::new(AtomicUsize::new(0));
    let rejected_drops = Arc::new(AtomicUsize::new(0));
    let queued = child
        .try_send(owned(11, &queued_drops))
        .expect("fills root mailbox");
    let rejected = owned(22, &rejected_drops);
    let pointer = std::ptr::from_ref(rejected.0.as_ref());
    let error = child.try_send(rejected).expect_err("Full scoped admission");
    assert!(error.is_full());
    assert!(!error.is_store_stopped());
    assert!(!error.is_scope_ended());
    assert_eq!(std::ptr::from_ref(error.action().0.as_ref()), pointer);
    assert!(format!("{error:?}").starts_with("TryScopedSendError { kind: Full,"));
    assert_eq!(
        error.to_string().lines().next(),
        Some("scoped store ingress is full")
    );
    let backtrace = error.backtrace().to_string();
    assert!(error.to_string().ends_with(&backtrace));
    assert!(std::error::Error::source(&error).is_none());
    assert_eq!(rejected_drops.load(Ordering::SeqCst), 0);
    assert_eq!(queued_drops.load(Ordering::SeqCst), 0);
    assert_eq!(embeds.load(Ordering::SeqCst), 0);
    assert_eq!(*fixture.reductions.lock().unwrap(), []);
    let recovered = error.into_action();
    assert_eq!(std::ptr::from_ref(recovered.0.as_ref()), pointer);
    assert_eq!(recovered.0.token, 22);
    assert_eq!(rejected_drops.load(Ordering::SeqCst), 0);
    drop(recovered);
    assert_eq!(rejected_drops.load(Ordering::SeqCst), 1);
    fixture
        .host
        .run_until_stalled()
        .expect("queued child action");
    complete(queued);
    assert_eq!(embeds.load(Ordering::SeqCst), 1);
    assert_eq!(*fixture.reductions.lock().unwrap(), [11]);
    assert_eq!(queued_drops.load(Ordering::SeqCst), 1);
    fixture.shutdown();
    assert_eq!(rejected_drops.load(Ordering::SeqCst), 1);
}

#[test]
fn scoped_stopped_recovers_move_only_action_without_embedding_or_reduction() {
    // Catches StoreStopped misclassified as ScopeEnded or eager rejected embedding.
    let fixture = Fixture::new(1);
    let embeds = Arc::new(AtomicUsize::new(0));
    let child = required_child(&fixture.store, &embeds);
    fixture.shutdown();
    let drops = Arc::new(AtomicUsize::new(0));
    let action = owned(7, &drops);
    let pointer = std::ptr::from_ref(action.0.as_ref());
    let error = child.try_send(action).expect_err("root StoreStopped");
    assert!(error.is_store_stopped());
    assert!(!error.is_scope_ended());
    assert!(!error.is_full());
    assert_eq!(std::ptr::from_ref(error.action().0.as_ref()), pointer);
    assert!(format!("{error:?}").starts_with("TryScopedSendError { kind: StoreStopped,"));
    assert_eq!(
        error.to_string().lines().next(),
        Some("root store stopped before scoped action admission")
    );
    assert!(std::error::Error::source(&error).is_none());
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    let recovered = error.into_action();
    assert_eq!(std::ptr::from_ref(recovered.0.as_ref()), pointer);
    assert_eq!(recovered.0.token, 7);
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    drop(recovered);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert_eq!(embeds.load(Ordering::SeqCst), 0);
    assert_eq!(*fixture.reductions.lock().unwrap(), []);
}

#[test]
fn ended_registered_optional_scope_recovers_move_only_action_without_embedding() {
    // Catches stale lifetime admission or treating a live root as StoreStopped.
    let fixture = Fixture::new(1);
    let embeds = Arc::new(AtomicUsize::new(0));
    let counted_embeds = Arc::clone(&embeds);
    let child = futures_executor::block_on(fixture.host.run(fixture.store.scope_some(
        OptionalStatePath::new(
            |state: &State| state.optional_child.as_ref(),
            |state: &mut State| state.optional_child.as_mut(),
        ),
        ActionPath::new(
            |action: &Action| match action {
                Action::Owned(action) => Some(action),
                Action::RemoveChild => None,
            },
            move |action| {
                counted_embeds.fetch_add(1, Ordering::SeqCst);
                Action::Owned(action)
            },
        ),
    )))
    .expect("registration drive")
    .expect("present optional child is registered");
    let removal = fixture
        .store
        .try_send(Action::RemoveChild)
        .expect("remove child admission");
    fixture
        .host
        .run_until_stalled()
        .expect("invalidate registered lifetime");
    complete(removal);
    assert!(!fixture.store.is_closed());
    assert_eq!(*fixture.reductions.lock().unwrap(), [0]);
    let drops = Arc::new(AtomicUsize::new(0));
    let action = owned(7, &drops);
    let pointer = std::ptr::from_ref(action.0.as_ref());
    let error = child.try_send(action).expect_err("ended optional lifetime");
    assert!(error.is_scope_ended());
    assert!(!error.is_store_stopped());
    assert!(!error.is_full());
    assert_eq!(std::ptr::from_ref(error.action().0.as_ref()), pointer);
    assert!(format!("{error:?}").starts_with("TryScopedSendError { kind: ScopeEnded,"));
    assert_eq!(
        error.to_string().lines().next(),
        Some("scoped store ended before action admission")
    );
    assert!(std::error::Error::source(&error).is_none());
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    let recovered = error.into_action();
    assert_eq!(std::ptr::from_ref(recovered.0.as_ref()), pointer);
    assert_eq!(recovered.0.token, 7);
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    drop(recovered);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert_eq!(embeds.load(Ordering::SeqCst), 0);
    assert_eq!(*fixture.reductions.lock().unwrap(), [0]);
    fixture.shutdown();
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[test]
fn root_admission_is_fifo_for_distinct_actions_before_host_progress() {
    // Catches queue reversal/reordering that a commutative sum cannot detect.
    let fixture = Fixture::new(3);
    let first_drops = Arc::new(AtomicUsize::new(0));
    let second_drops = Arc::new(AtomicUsize::new(0));
    let third_drops = Arc::new(AtomicUsize::new(0));
    let first = fixture
        .store
        .try_send(Action::Owned(owned(3, &first_drops)))
        .expect("first");
    let second = fixture
        .store
        .try_send(Action::Owned(owned(1, &second_drops)))
        .expect("second");
    let third = fixture
        .store
        .try_send(Action::Owned(owned(2, &third_drops)))
        .expect("third");
    assert_eq!(*fixture.reductions.lock().unwrap(), []);
    assert_eq!(first_drops.load(Ordering::SeqCst), 0);
    assert_eq!(second_drops.load(Ordering::SeqCst), 0);
    assert_eq!(third_drops.load(Ordering::SeqCst), 0);
    fixture
        .host
        .run_until_stalled()
        .expect("ordered useful work");
    complete(first);
    complete(second);
    complete(third);
    assert_eq!(*fixture.reductions.lock().unwrap(), [3, 1, 2]);
    assert_eq!(first_drops.load(Ordering::SeqCst), 1);
    assert_eq!(second_drops.load(Ordering::SeqCst), 1);
    assert_eq!(third_drops.load(Ordering::SeqCst), 1);
    fixture.shutdown();
    assert_eq!(*fixture.reductions.lock().unwrap(), [3, 1, 2]);
}

/// Observes cleanup when the ordinary shutdown reply notifies its waiter, not
/// merely after the actor's entire poll has returned. It does not drive the host.
struct ShutdownWake {
    earlier_drops: Arc<AtomicUsize>,
    later_drops: Arc<AtomicUsize>,
    state_drops: Arc<AtomicUsize>,
    reducer_drops: Arc<AtomicUsize>,
    snapshots: Mutex<Vec<[usize; 4]>>,
}

impl Wake for ShutdownWake {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.snapshots.lock().unwrap().push([
            self.earlier_drops.load(Ordering::SeqCst),
            self.later_drops.load(Ordering::SeqCst),
            self.state_drops.load(Ordering::SeqCst),
            self.reducer_drops.load(Ordering::SeqCst),
        ]);
    }
}

#[test]
fn ordinary_shutdown_commits_earlier_rejects_later_and_cleans_up_before_ack() {
    // Catches terminal reordering, reduction of drained work, or acknowledgement
    // before queued payload/state/reducer release. This is not state extraction.
    let fixture = Fixture::new(3);
    let earlier_drops = Arc::new(AtomicUsize::new(0));
    let later_drops = Arc::new(AtomicUsize::new(0));
    let earlier = fixture
        .store
        .try_send(Action::Owned(owned(1, &earlier_drops)))
        .expect("earlier admission");
    let mut earlier_finish = Box::pin(earlier.finish());
    assert!(poll(earlier_finish.as_mut()).is_pending());
    let observer = Arc::new(ShutdownWake {
        earlier_drops: Arc::clone(&earlier_drops),
        later_drops: Arc::clone(&later_drops),
        state_drops: Arc::clone(&fixture.state_drops),
        reducer_drops: Arc::clone(&fixture.reducer_drops),
        snapshots: Mutex::new(Vec::new()),
    });
    let waker = Waker::from(Arc::clone(&observer));
    let mut shutdown = Box::pin(fixture.store.shutdown());
    assert!(
        shutdown
            .as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_pending()
    );
    let later = fixture
        .store
        .try_send(Action::Owned(owned(2, &later_drops)))
        .expect("later admission behind terminal");
    let mut later_finish = Box::pin(later.finish());
    assert!(poll(later_finish.as_mut()).is_pending());
    assert_eq!(*fixture.reductions.lock().unwrap(), []);
    assert_eq!(earlier_drops.load(Ordering::SeqCst), 0);
    assert_eq!(later_drops.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.state_drops.load(Ordering::SeqCst), 0);
    assert_eq!(fixture.reducer_drops.load(Ordering::SeqCst), 0);
    assert!(observer.snapshots.lock().unwrap().is_empty());

    fixture
        .host
        .run_until_stalled()
        .expect("earlier, terminal, later processing");
    assert_eq!(*fixture.reductions.lock().unwrap(), [1]);
    assert_eq!(earlier_drops.load(Ordering::SeqCst), 1);
    assert_eq!(later_drops.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.state_drops.load(Ordering::SeqCst), 1);
    assert_eq!(fixture.reducer_drops.load(Ordering::SeqCst), 1);
    let snapshots = observer.snapshots.lock().unwrap();
    assert!(
        !snapshots.is_empty(),
        "shutdown waiter was actually notified"
    );
    for snapshot in snapshots.iter() {
        assert_eq!(
            *snapshot,
            [1, 1, 1, 1],
            "cleanup precedes reply notification"
        );
    }
    drop(snapshots);
    let Poll::Ready(Ok(task)) = poll(earlier_finish.as_mut()) else {
        panic!("earlier input must commit before the terminal command");
    };
    futures_executor::block_on(task.finish());
    assert!(task.is_finished());
    assert!(matches!(poll(later_finish.as_mut()), Poll::Ready(Err(_))));
    assert!(matches!(poll(shutdown.as_mut()), Poll::Ready(Ok(()))));
    assert!(fixture.store.is_closed());
    futures_executor::block_on(fixture.host.drain_deadlines(0))
        .expect("no scheduler tasks or timers remain after ordinary shutdown");
}
