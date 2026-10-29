//! Checks terminal state transfer, admission order and cleanup before handoff.

use std::cell::Cell;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Wake, Waker};

use rca_core::{
    ActionPath, DependencyKey, Effect, EffectSender, ReduceContext, Reducer, StatePath, Store,
};
use rca_test::DeterministicRuntime;

/// Tracks destruction of state with an owned, `Send`-only product.
struct State {
    product: Box<Cell<usize>>,
    drops: Arc<AtomicUsize>,
}

impl Drop for State {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

/// Commits a value, starts owned work, or triggers a controlled reducer panic.
enum Action {
    Add(usize),
    Start,
    Panic,
}

/// Mutates the owned product without cloning its allocation.
struct Feature;

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
            Action::Add(value) => state.product.set(state.product.get() + value),
            Action::Start => {}
            Action::Panic => panic!("extraction reducer panic"),
        }
        Effect::none()
    }
}

#[test]
fn extracts_exact_move_only_state() {
    let host = DeterministicRuntime::new();
    let drops = Arc::new(AtomicUsize::new(0));
    let product = Box::new(Cell::new(4));
    let pointer = std::ptr::from_ref(product.as_ref());
    let store = Store::new(
        State {
            product,
            drops: Arc::clone(&drops),
        },
        Feature,
        host.runtime(),
    );
    let state = futures_executor::block_on(host.run(async move {
        store
            .send(Action::Add(3))
            .await
            .expect("commit")
            .finish()
            .await;
        store.shutdown_into_state().await.expect("owned state")
    }))
    .expect("drive");
    assert_eq!(std::ptr::from_ref(state.product.as_ref()), pointer);
    assert_eq!(state.product.get(), 7);
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    drop(state);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

/// Observes one readiness step, without making a wake-delivery assertion.
fn poll<F: Future>(future: Pin<&mut F>) -> Poll<F::Output> {
    future.poll(&mut Context::from_waker(Waker::noop()))
}

/// Creates a store with explicit mailbox capacity and an independent drop counter.
fn fixture(
    host: &DeterministicRuntime,
    capacity: usize,
) -> (Store<State, Action>, Arc<AtomicUsize>) {
    let drops = Arc::new(AtomicUsize::new(0));
    let store = Store::builder(
        State {
            product: Box::new(Cell::new(4)),
            drops: Arc::clone(&drops),
        },
        Feature,
        host.runtime(),
    )
    .mailbox_capacity(capacity)
    .build()
    .expect("store");
    (store, drops)
}

#[test]
fn two_admitted_extractions_transfer_only_to_first() {
    let host = DeterministicRuntime::new();
    let (store, drops) = fixture(&host, 4);
    let survivor = store.clone();
    let mut first = Box::pin(store.clone().shutdown_into_state());
    let mut second = Box::pin(store.shutdown_into_state());
    assert!(poll(first.as_mut()).is_pending());
    assert!(poll(second.as_mut()).is_pending());
    host.run_until_stalled().expect("terminal work");
    let Poll::Ready(Ok(state)) = poll(first.as_mut()) else {
        panic!("first must win")
    };
    assert!(matches!(poll(second.as_mut()), Poll::Ready(Err(_))));
    assert!(survivor.is_closed());
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    drop(state);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[test]
fn ordinary_shutdown_before_extraction_drops_state_before_ack() {
    let host = DeterministicRuntime::new();
    let (store, drops) = fixture(&host, 4);
    let mut shutdown = Box::pin(store.shutdown());
    let mut extraction = Box::pin(store.clone().shutdown_into_state());
    assert!(poll(shutdown.as_mut()).is_pending());
    assert!(poll(extraction.as_mut()).is_pending());
    host.run_until_stalled().expect("terminal work");
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert!(matches!(poll(shutdown.as_mut()), Poll::Ready(Ok(()))));
    assert!(matches!(poll(extraction.as_mut()), Poll::Ready(Err(_))));
    assert!(store.is_closed());
}

#[test]
fn extraction_before_ordinary_shutdown_wins() {
    let host = DeterministicRuntime::new();
    let (store, drops) = fixture(&host, 4);
    let mut extraction = Box::pin(store.clone().shutdown_into_state());
    let mut shutdown = Box::pin(store.shutdown());
    assert!(poll(extraction.as_mut()).is_pending());
    assert!(poll(shutdown.as_mut()).is_pending());
    host.run_until_stalled().expect("terminal work");
    let Poll::Ready(Ok(state)) = poll(extraction.as_mut()) else {
        panic!("extraction wins")
    };
    // An already-admitted shutdown behind the winner keeps its existing error outcome.
    assert!(matches!(poll(shutdown.as_mut()), Poll::Ready(Err(_))));
    assert!(matches!(
        poll(Box::pin(store.shutdown()).as_mut()),
        Poll::Ready(Ok(()))
    ));
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    drop(state);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[test]
fn terminal_position_commits_earlier_and_rejects_later_admitted_actions() {
    let host = DeterministicRuntime::new();
    let (store, drops) = fixture(&host, 4);
    let earlier = store.try_send(Action::Add(3)).expect("earlier admission");
    let mut extraction = Box::pin(store.clone().shutdown_into_state());
    assert!(poll(extraction.as_mut()).is_pending());
    let later = store.try_send(Action::Add(100)).expect("later admission");
    host.run_until_stalled().expect("terminal work");
    let Poll::Ready(Ok(state)) = poll(extraction.as_mut()) else {
        panic!("extraction")
    };
    let _ = futures_executor::block_on(earlier.finish()).expect("earlier committed");
    futures_executor::block_on(later.finish()).expect_err("later rejected");
    assert_eq!(state.product.get(), 7);
    assert!(store.is_closed());
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    drop(state);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[test]
fn unpolled_extraction_only_drops_its_owner() {
    let host = DeterministicRuntime::new();
    let (store, drops) = fixture(&host, 1);
    drop(store.clone().shutdown_into_state());
    host.run_until_stalled().expect("still running");
    assert!(!store.is_closed());
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    let mut closed = Box::pin(store.closed());
    drop(store.shutdown_into_state());
    host.run_until_stalled().expect("last owner cleanup");
    assert!(poll(closed.as_mut()).is_ready());
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[test]
fn cancellation_while_mailbox_full_does_not_submit_terminal_command() {
    let host = DeterministicRuntime::new();
    let (store, drops) = fixture(&host, 1);
    let receipt = store.try_send(Action::Add(3)).expect("fill mailbox");
    let mut extraction = Box::pin(store.clone().shutdown_into_state());
    assert!(poll(extraction.as_mut()).is_pending());
    drop(extraction);
    host.run_until_stalled().expect("earlier action only");
    let _ = futures_executor::block_on(receipt.finish()).expect("queued action committed");
    assert!(!store.is_closed());
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    let state = futures_executor::block_on(host.run(store.shutdown_into_state()))
        .expect("drive")
        .expect("no terminal submitted");
    assert_eq!(state.product.get(), 7);
    drop(state);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[test]
fn abandoned_admitted_request_still_closes_with_surviving_owner() {
    let host = DeterministicRuntime::new();
    let (store, drops) = fixture(&host, 1);
    let mut extraction = Box::pin(store.clone().shutdown_into_state());
    assert!(poll(extraction.as_mut()).is_pending());
    drop(extraction);
    host.run_until_stalled().expect("terminal work");
    assert!(store.is_closed());
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert!(futures_executor::block_on(store.shutdown_into_state()).is_err());
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[test]
fn cancellation_before_admission_as_final_owner_aborts_queued_work() {
    let host = DeterministicRuntime::new();
    let (store, drops) = fixture(&host, 1);
    let mut closed = Box::pin(store.closed());
    let receipt = store.try_send(Action::Add(3)).expect("fill mailbox");
    let mut extraction = Box::pin(store.shutdown_into_state());
    assert!(poll(extraction.as_mut()).is_pending());
    drop(extraction);
    host.run_until_stalled().expect("abortive cleanup");
    assert!(poll(closed.as_mut()).is_ready());
    futures_executor::block_on(receipt.finish()).expect_err("queued action aborted");
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[test]
fn abandoned_admitted_final_owner_drops_state_once() {
    let host = DeterministicRuntime::new();
    let (store, drops) = fixture(&host, 1);
    let mut closed = Box::pin(store.closed());
    let mut extraction = Box::pin(store.shutdown_into_state());
    assert!(poll(extraction.as_mut()).is_pending());
    drop(extraction);
    host.run_until_stalled().expect("abortive work");
    assert!(poll(closed.as_mut()).is_ready());
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[test]
fn receiver_abandonment_after_cleanup_drops_unclaimed_state() {
    let host = DeterministicRuntime::new();
    let (store, drops) = fixture(&host, 1);
    let survivor = store.clone();
    let mut extraction = Box::pin(store.shutdown_into_state());
    assert!(poll(extraction.as_mut()).is_pending());
    host.run_until_stalled().expect("handoff queued");
    assert!(survivor.is_closed());
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    drop(extraction);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[test]
fn host_loss_does_not_fabricate_output() {
    let host = DeterministicRuntime::new();
    let (store, drops) = fixture(&host, 1);
    let survivor = store.clone();
    let mut extraction = Box::pin(store.shutdown_into_state());
    assert!(poll(extraction.as_mut()).is_pending());
    drop(host);
    assert!(matches!(poll(extraction.as_mut()), Poll::Ready(Err(_))));
    assert!(survivor.is_closed());
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

/// Counts reducer, dependency or effect resource destruction.
struct DropProbe(Arc<AtomicUsize>);

impl Drop for DropProbe {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

/// Requires an explicit dependency whose destruction precedes state handoff.
struct ProbeKey;

/// Records cleanup counters at the wake that announces a completed handoff.
struct HandoffWake {
    reducer: Arc<AtomicUsize>,
    dependency: Arc<AtomicUsize>,
    state: Arc<AtomicUsize>,
    observed: std::sync::Mutex<Vec<(usize, usize, usize)>>,
}

impl Wake for HandoffWake {
    fn wake(self: Arc<Self>) {
        self.observed.lock().expect("wake observations").push((
            self.reducer.load(Ordering::SeqCst),
            self.dependency.load(Ordering::SeqCst),
            self.state.load(Ordering::SeqCst),
        ));
    }
}

impl DependencyKey for ProbeKey {
    type Value = Arc<DropProbe>;

    fn live_value() -> Self::Value {
        panic!("test supplies dependency")
    }
}

/// Owns reducer resources and produces a retained sender plus a delayed action.
struct CleanupFeature {
    _probe: DropProbe,
    effect_drops: Arc<AtomicUsize>,
    sender: std::sync::mpsc::Sender<EffectSender<Action>>,
}

impl Reducer for CleanupFeature {
    type State = State;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut State,
        action: &Action,
        context: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        if matches!(action, Action::Start) {
            let sender = self.sender.clone();
            let probe = DropProbe(Arc::clone(&self.effect_drops));
            return Effect::merge([
                Effect::run(move |effect_sender| async move {
                    let _probe = probe;
                    sender.send(effect_sender).expect("retain effect sender");
                    std::future::pending::<()>().await;
                }),
                Effect::send_after(std::time::Duration::from_secs(5), Action::Add(100)),
            ]);
        }
        Feature.reduce(state, action, context)
    }
}

#[test]
fn cleanup_precedes_handoff_and_revokes_old_work() {
    let host = DeterministicRuntime::new();
    let drops = Arc::new(AtomicUsize::new(0));
    let reducer_drops = Arc::new(AtomicUsize::new(0));
    let dependency_drops = Arc::new(AtomicUsize::new(0));
    let effect_drops = Arc::new(AtomicUsize::new(0));
    let (sender, receive) = std::sync::mpsc::channel();
    let store = Store::builder(
        State {
            product: Box::new(Cell::new(4)),
            drops: Arc::clone(&drops),
        },
        CleanupFeature {
            _probe: DropProbe(Arc::clone(&reducer_drops)),
            effect_drops: Arc::clone(&effect_drops),
            sender,
        },
        host.runtime(),
    )
    .with_dependency::<ProbeKey>(Arc::new(DropProbe(Arc::clone(&dependency_drops))))
    .build()
    .expect("store");
    let (mut observation, scoped, task) = futures_executor::block_on(host.run(async {
        let observation = store
            .observe(|state| state.product.get())
            .await
            .expect("observe");
        let scoped = store.scope(
            StatePath::new(
                |state: &State| state.product.as_ref(),
                |state: &mut State| state.product.as_mut(),
            ),
            ActionPath::new(
                |action: &Action| {
                    if let Action::Add(value) = action {
                        Some(value)
                    } else {
                        None
                    }
                },
                Action::Add,
            ),
        );
        let task = store.send(Action::Start).await.expect("start");
        (observation, scoped, task)
    }))
    .expect("drive setup");
    host.run_until_stalled().expect("effect and timer pending");
    let old_sender = receive.try_recv().expect("effect started");
    assert_eq!(host.pending_sleeps().len(), 1);
    assert_eq!(effect_drops.load(Ordering::SeqCst), 0);
    let survivor = store.clone();
    let wake = Arc::new(HandoffWake {
        reducer: Arc::clone(&reducer_drops),
        dependency: Arc::clone(&dependency_drops),
        state: Arc::clone(&drops),
        observed: std::sync::Mutex::new(Vec::new()),
    });
    let waker = Waker::from(Arc::clone(&wake));
    let mut extraction = Box::pin(store.shutdown_into_state());
    assert!(
        extraction
            .as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_pending()
    );
    host.run_until_stalled()
        .expect("terminal and ready abort work");
    assert_eq!(
        *wake.observed.lock().expect("wake observations"),
        [(1, 1, 0)]
    );
    let Poll::Ready(Ok(state)) = poll(extraction.as_mut()) else {
        panic!("handoff")
    };
    assert!(survivor.is_closed());
    assert_eq!(reducer_drops.load(Ordering::SeqCst), 1);
    assert_eq!(dependency_drops.load(Ordering::SeqCst), 1);
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    assert!(futures_executor::block_on(old_sender.send(Action::Add(100))).is_err());
    futures_executor::block_on(scoped.send(100)).expect_err("scope closed");
    // Consume the initial snapshot, if still buffered, then prove closure.
    let _ = futures_executor::block_on(observation.next());
    assert!(futures_executor::block_on(observation.next()).is_none());
    host.run_until_stalled()
        .expect("poll cooperative aborted wrapper");
    assert_eq!(effect_drops.load(Ordering::SeqCst), 1);
    assert!(task.is_finished());
    assert!(host.pending_sleeps().is_empty());
    host.advance(std::time::Duration::from_secs(5))
        .expect("cancelled timer");
    assert_eq!(state.product.get(), 4);
    drop(state);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[test]
fn reducer_panic_propagates_without_fabricated_output() {
    let host = DeterministicRuntime::new();
    let (store, drops) = fixture(&host, 4);
    let survivor = store.clone();
    let receipt = store.try_send(Action::Panic).expect("panic admission");
    let mut extraction = Box::pin(store.shutdown_into_state());
    assert!(poll(extraction.as_mut()).is_pending());
    let failure = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        host.run_until_stalled().expect("drive");
    }))
    .expect_err("original reducer panic propagates");
    assert_eq!(
        failure.downcast_ref::<&str>(),
        Some(&"extraction reducer panic")
    );
    assert!(survivor.is_closed());
    assert!(matches!(poll(extraction.as_mut()), Poll::Ready(Err(_))));
    futures_executor::block_on(receipt.finish()).expect_err("panicked action not committed");
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}
