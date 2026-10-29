//! Owned client clocks share test time without retaining actor authority.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use futures_executor::block_on;
use rca_core::{
    Clock, DependencyKey, Effect, MonotonicInstant, ReduceContext, Reducer, with_dependency,
};
use rca_test::{DeterministicRuntime, TestStore};

/// Client completion precedes the independently scheduled actor action.
const CLIENT_DELAY: Duration = Duration::from_millis(10);

/// A separate deadline detects accidental sharing of timer state.
const ACTOR_DELAY: Duration = Duration::from_millis(15);

/// An owned client whose clock and captured resource outlive each request call.
struct Client {
    /// Time supplied by the test store or an intentional wrong-clock control.
    clock: Clock,
    /// Identifies which dependency override supplied the client.
    label: String,
    /// Reference counts reveal whether cancelled requests release captures.
    capture: Arc<()>,
}

impl Client {
    /// Creates a client with its own observable capture lifetime.
    fn new(clock: Clock, label: &str) -> Self {
        Self {
            clock,
            label: label.into(),
            capture: Arc::new(()),
        }
    }

    /// Fixes the deadline now and returns a request that does not borrow the client.
    fn request(&self) -> impl Future<Output = Option<Action>> + Send + 'static {
        let wait = self.clock.sleep(CLIENT_DELAY);
        let label = self.label.clone();
        let capture = Arc::clone(&self.capture);
        async move {
            wait.await;
            drop(capture);
            Some(Action::Client(label))
        }
    }
}

/// Requires explicit construction so an accidental live fallback fails the test.
struct ClientKey;

impl DependencyKey for ClientKey {
    type Value = Client;
    fn live_value() -> Client {
        panic!("client must be explicitly constructed")
    }
}

/// Starts client or actor work and records which dependency supplied its output.
#[derive(Debug, PartialEq)]
enum Action {
    Start,
    StartBoth,
    Wait,
    Client(String),
    Actor,
    Read,
}

/// Routes owned requests and actor timers into one ordered output log.
struct Feature;

impl Reducer for Feature {
    type State = Vec<String>;
    type Action = Action;
    fn reduce(
        &mut self,
        state: &mut Vec<String>,
        action: &Action,
        context: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        match action {
            Action::Start => Effect::future(context.dependency::<ClientKey>().request()),
            Action::StartBoth => Effect::merge([
                Effect::future(context.dependency::<ClientKey>().request()),
                Effect::send_after(ACTOR_DELAY, Action::Actor),
            ]),
            Action::Wait => Effect::send_after(ACTOR_DELAY, Action::Actor),
            Action::Client(label) => {
                state.push(label.clone());
                Effect::none()
            }
            Action::Actor => {
                state.push("actor".into());
                Effect::none()
            }
            Action::Read => {
                // This override only reads the label; its temporary host is never driven.
                let label = context.with_dependency_override::<ClientKey, _>(
                    Client::new(DeterministicRuntime::new().runtime().clock(), "action"),
                    |context| context.dependency::<ClientKey>().label.clone(),
                );
                state.push(label);
                Effect::none()
            }
        }
    }
}

/// Polls once; these tests advance time and poll again explicitly.
fn poll(wait: &mut rca_core::RuntimeFuture) -> Poll<()> {
    wait.as_mut().poll(&mut Context::from_waker(Waker::noop()))
}

#[test]
fn public_clock_is_clone_send_sync_and_returns_owned_send_waits() {
    fn traits<T: Clone + Send + Sync + std::fmt::Debug>() {}
    fn owned_send<T: Future<Output = ()> + Send + 'static>(_: &T) {}
    traits::<Clock>();
    let host = DeterministicRuntime::new();
    let clock = host.runtime().clock();
    let mut wait = clock.clone().sleep(Duration::ZERO);
    owned_send(&wait);
    drop(clock);
    assert_eq!(poll(&mut wait), Poll::Ready(()));
}

#[test]
fn relative_wait_fixes_deadline_at_invocation_not_first_poll() {
    let host = DeterministicRuntime::new();
    let clock = host.runtime().clock();
    let mut wait = clock.sleep(CLIENT_DELAY);
    host.advance(Duration::from_millis(9)).unwrap();
    assert_eq!(clock.now(), host.now());
    assert_eq!(poll(&mut wait), Poll::Pending);
    assert_eq!(
        host.pending_sleeps()[0].deadline(),
        MonotonicInstant::from_elapsed(CLIENT_DELAY)
    );
    host.advance(Duration::from_millis(1)).unwrap();
    assert_eq!(poll(&mut wait), Poll::Ready(()));
    assert!(host.pending_sleeps().is_empty());
}

#[test]
fn elapsed_and_zero_waits_are_ready_on_first_poll() {
    let host = DeterministicRuntime::new();
    let clock = host.runtime().clock();
    let mut delayed = clock.sleep(CLIENT_DELAY);
    host.advance(CLIENT_DELAY).unwrap();
    for mut wait in [
        delayed.as_mut(),
        clock.sleep(Duration::ZERO).as_mut(),
        clock.sleep_until(MonotonicInstant::ZERO).as_mut(),
    ] {
        assert_eq!(
            wait.as_mut().poll(&mut Context::from_waker(Waker::noop())),
            Poll::Ready(())
        );
    }
    assert!(host.pending_sleeps().is_empty());
}

#[test]
fn maximum_duration_saturates_and_never_completes_early() {
    let host = DeterministicRuntime::new();
    host.advance(Duration::from_secs(1)).unwrap();
    let mut wait = host.runtime().clock().sleep(Duration::MAX);
    assert_eq!(poll(&mut wait), Poll::Pending);
    assert_eq!(host.pending_sleeps()[0].deadline(), MonotonicInstant::MAX);
    host.advance(Duration::from_secs(1)).unwrap();
    assert_eq!(poll(&mut wait), Poll::Pending);
    host.advance(Duration::MAX).unwrap();
    assert_eq!(poll(&mut wait), Poll::Ready(()));
}

#[test]
fn dropping_unpolled_and_registered_waits_releases_timers() {
    let host = DeterministicRuntime::new();
    let clock = host.runtime().clock();
    drop(clock.sleep(CLIENT_DELAY));
    assert!(host.pending_sleeps().is_empty());
    let mut wait = clock.sleep_until(MonotonicInstant::MAX);
    assert_eq!(poll(&mut wait), Poll::Pending);
    assert_eq!(host.pending_sleeps().len(), 1);
    drop(wait);
    assert!(host.pending_sleeps().is_empty());
}

#[test]
fn injected_owned_client_and_actor_obey_distinct_virtual_deadlines() {
    block_on(async {
        let mut store = TestStore::builder(Vec::new(), Feature)
            .with_clock_dependency::<ClientKey>(|clock| Client::new(clock, "client"))
            .build();
        let task = store.send(Action::StartBoth).await.unwrap();
        store.advance(Duration::from_millis(9)).await.unwrap();
        assert!(
            store
                .receive(|_| true)
                .await
                .unwrap_err()
                .is_no_received_action()
        );
        store.assert_state_eq(Vec::new()).await.unwrap();
        store.advance(Duration::from_millis(1)).await.unwrap();
        let _ = store
            .receive_with(
                |a| *a == Action::Client("client".into()),
                |s| s.push("client".into()),
            )
            .await
            .unwrap();
        store.advance(Duration::from_millis(4)).await.unwrap();
        assert!(
            store
                .receive(|_| true)
                .await
                .unwrap_err()
                .is_no_received_action()
        );
        store.advance(Duration::from_millis(1)).await.unwrap();
        let _ = store
            .receive_with(|a| *a == Action::Actor, |s| s.push("actor".into()))
            .await
            .unwrap();
        task.finish().await.unwrap();
        store.finish().await.unwrap();
    });
}

#[test]
fn factory_runs_once_at_build_accepts_send_only_owned_captures() {
    /// A non-`Sync`, non-`Clone` capture that the factory must consume once.
    struct Owned(std::cell::Cell<String>);

    impl Owned {
        /// Moves out the label instead of requiring a shared reference.
        fn into_label(self) -> String {
            self.0.into_inner()
        }
    }
    fn send<T: Send>(_: &T) {}
    block_on(async {
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&calls);
        let owned = Owned(std::cell::Cell::new("deferred".into()));
        let builder = TestStore::projected_builder(Vec::new(), Feature, Vec::clone)
            .with_clock_dependency::<ClientKey>(move |clock| {
                seen.fetch_add(1, Ordering::SeqCst);
                assert_eq!(clock.now(), MonotonicInstant::ZERO);
                Client {
                    clock,
                    label: owned.into_label(),
                    capture: Arc::new(()),
                }
            });
        // This consumed, non-Clone factory owns Send-only data. The builder
        // remains Send; completed Store/TestStore retain their Sync law.
        send(&builder);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let mut store = builder.build();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let _ = store
            .send_with(Action::Read, |s| s.push("deferred".into()))
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        store.finish().await.unwrap();
    });
}

#[test]
fn dependency_registration_order_is_preserved_in_both_directions() {
    for deferred_last in [false, true] {
        block_on(async {
            let host = DeterministicRuntime::new();
            let builder = TestStore::builder(Vec::new(), Feature);
            let builder = if deferred_last {
                builder
                    .with_dependency::<ClientKey>(Client::new(host.runtime().clock(), "ordinary"))
                    .with_clock_dependency::<ClientKey>(|clock| Client::new(clock, "deferred"))
            } else {
                builder
                    .with_clock_dependency::<ClientKey>(|clock| Client::new(clock, "deferred"))
                    .with_dependency::<ClientKey>(Client::new(host.runtime().clock(), "ordinary"))
            };
            let mut store = builder.build();
            let _ = store
                .send_with(Action::Read, move |s| {
                    s.push(
                        if deferred_last {
                            "deferred"
                        } else {
                            "ordinary"
                        }
                        .into(),
                    );
                })
                .await
                .unwrap();
            store.finish().await.unwrap();
        });
    }
}

#[test]
fn finish_reports_an_external_sleep_without_actor_owned_work() {
    let clock = Arc::new(std::sync::OnceLock::new());
    let captured = Arc::clone(&clock);
    let mut store = TestStore::builder(Vec::new(), Feature)
        .with_clock_dependency::<ClientKey>(move |clock| {
            captured.set(clock.clone()).unwrap();
            Client::new(clock, "external")
        })
        .build();
    let mut wait = clock.get().unwrap().sleep(CLIENT_DELAY);
    assert_eq!(poll(&mut wait), Poll::Pending);

    let error = block_on(store.finish()).unwrap_err();
    let diagnostics = error.finish_diagnostics().unwrap();
    assert!(!diagnostics.has_pending_action());
    let runtime = diagnostics.runtime();
    assert!(runtime.live_task_ids.is_empty());
    assert_eq!(runtime.cancellation_count, 0);
    assert!(runtime.timer_deadlines.is_empty());
    assert_eq!(runtime.observation_count, 0);
    assert_eq!(diagnostics.pending_sleeps().len(), 1);
    assert_eq!(
        diagnostics.pending_sleeps()[0].deadline(),
        MonotonicInstant::from_elapsed(CLIENT_DELAY)
    );
    assert_eq!(store.now(), MonotonicInstant::ZERO);

    drop(wait);
    block_on(store.finish()).unwrap();
}

#[test]
fn deferred_test_dependency_beats_action_feature_and_root_overrides() {
    block_on(async {
        let host = DeterministicRuntime::new();
        let reducer = with_dependency::<ClientKey, _>(
            Client::new(host.runtime().clock(), "feature"),
            Feature,
        );
        let mut store = TestStore::builder(Vec::new(), reducer)
            .with_clock_dependency::<ClientKey>(|clock| Client::new(clock, "test"))
            .with_store_dependency::<ClientKey>(Client::new(host.runtime().clock(), "root"))
            .build();
        let _ = store
            .send_with(Action::Read, |s| s.push("test".into()))
            .await
            .unwrap();
        store.finish().await.unwrap();
    });
}

#[test]
fn independent_test_store_clocks_do_not_advance_each_other() {
    block_on(async {
        let build = || {
            TestStore::builder(Vec::new(), Feature)
                .with_clock_dependency::<ClientKey>(|clock| Client::new(clock, "client"))
                .build()
        };
        let mut first = build();
        let mut second = build();
        let a = first.send(Action::Start).await.unwrap();
        let b = second.send(Action::Start).await.unwrap();
        first.advance(CLIENT_DELAY).await.unwrap();
        let _ = first
            .receive_with(|_| true, |s| s.push("client".into()))
            .await
            .unwrap();
        assert_eq!(second.now(), MonotonicInstant::ZERO);
        assert!(
            second
                .receive(|_| true)
                .await
                .unwrap_err()
                .is_no_received_action()
        );
        second.advance(CLIENT_DELAY).await.unwrap();
        let _ = second
            .receive_with(|_| true, |s| s.push("client".into()))
            .await
            .unwrap();
        a.finish().await.unwrap();
        b.finish().await.unwrap();
        first.finish().await.unwrap();
        second.finish().await.unwrap();
    });
}

#[test]
fn wrong_clock_negative_control_cannot_receive_and_cancels_without_hanging() {
    block_on(async {
        let wrong = DeterministicRuntime::new();
        let client = Client::new(wrong.runtime().clock(), "wrong");
        let capture = Arc::clone(&client.capture);
        let mut store = TestStore::builder(Vec::new(), Feature)
            .with_dependency::<ClientKey>(client)
            .build();
        let task = store.send(Action::Start).await.unwrap();
        assert_eq!(wrong.pending_sleeps().len(), 1);
        store.advance(Duration::from_secs(1)).await.unwrap();
        assert!(
            store
                .receive(|_| true)
                .await
                .unwrap_err()
                .is_no_received_action()
        );
        assert_eq!(wrong.now(), MonotonicInstant::ZERO);
        assert_eq!(Arc::strong_count(&capture), 3);
        task.cancel().await.unwrap();
        assert!(wrong.pending_sleeps().is_empty());
        assert_eq!(Arc::strong_count(&capture), 2);
        wrong.advance(Duration::from_secs(1)).unwrap();
        assert!(
            store
                .receive(|_| true)
                .await
                .unwrap_err()
                .is_no_received_action()
        );
        task.finish().await.unwrap();
        store.finish().await.unwrap();
    });
}

#[test]
fn cancelling_owned_sleeping_client_releases_timer_capture_and_prevents_late_action() {
    block_on(async {
        let capture = Arc::new(());
        let captured = Arc::clone(&capture);
        let mut store = TestStore::builder(Vec::new(), Feature)
            .with_clock_dependency::<ClientKey>(move |clock| Client {
                clock,
                label: "client".into(),
                capture: captured,
            })
            .build();
        let task = store.send(Action::Start).await.unwrap();
        let sentinel = store.send(Action::Wait).await.unwrap();
        let error = store.finish().await.unwrap_err();
        assert_eq!(
            error.finish_diagnostics().unwrap().pending_sleeps().len(),
            2
        );
        assert_eq!(Arc::strong_count(&capture), 3);
        task.cancel().await.unwrap();
        assert_eq!(Arc::strong_count(&capture), 2);
        let error = store.finish().await.unwrap_err();
        let remaining = error.finish_diagnostics().unwrap().pending_sleeps();
        assert_eq!(remaining.len(), 1);
        assert_eq!(
            remaining[0].deadline(),
            MonotonicInstant::from_elapsed(ACTOR_DELAY)
        );
        sentinel.cancel().await.unwrap();
        store.advance(Duration::from_secs(1)).await.unwrap();
        assert!(
            store
                .receive(|_| true)
                .await
                .unwrap_err()
                .is_no_received_action()
        );
        task.finish().await.unwrap();
        store.finish().await.unwrap();
        assert_eq!(Arc::strong_count(&capture), 1);
    });
}
