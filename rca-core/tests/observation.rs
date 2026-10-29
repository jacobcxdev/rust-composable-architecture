#![expect(
    clippy::duplicate_mod,
    reason = "independently reusable test hosts each own the same private clock module"
)]

#[path = "support/thread_runtime.rs"]
mod support;

#[expect(dead_code, reason = "shutdown tests manually poll the actor")]
#[path = "support/mod.rs"]
mod manual_support;

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};
use std::task::{Context, Poll};
use std::time::Duration;

use rca_core::{Effect, ReduceContext, Reducer, Store};

use support::ThreadRuntime;

struct Counter;

impl Reducer for Counter {
    type State = usize;
    type Action = ();

    fn reduce(
        &mut self,
        state: &mut Self::State,
        _action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        *state += 1;
        Effect::none()
    }
}

struct MoveOnly(usize);

struct CloneProbe {
    value: usize,
    clones: Arc<AtomicUsize>,
}

impl Clone for CloneProbe {
    fn clone(&self) -> Self {
        self.clones.fetch_add(1, Ordering::AcqRel);
        Self {
            value: self.value,
            clones: Arc::clone(&self.clones),
        }
    }
}

impl PartialEq for CloneProbe {
    fn eq(&self, other: &Self) -> bool {
        self.value == other.value
    }
}

struct DropProbe {
    value: usize,
    live: Arc<AtomicUsize>,
    drops: Arc<AtomicUsize>,
}

impl DropProbe {
    fn new(value: usize, live: &Arc<AtomicUsize>, drops: &Arc<AtomicUsize>) -> Self {
        live.fetch_add(1, Ordering::AcqRel);
        Self {
            value,
            live: Arc::clone(live),
            drops: Arc::clone(drops),
        }
    }
}

impl Drop for DropProbe {
    fn drop(&mut self) {
        self.live.fetch_sub(1, Ordering::AcqRel);
        self.drops.fetch_add(1, Ordering::AcqRel);
    }
}

#[derive(Default)]
struct Gate {
    open: Mutex<bool>,
    changed: Condvar,
}

impl Gate {
    fn wait(&self) {
        let mut open = self.open.lock().expect("gate lock must not be poisoned");
        while !*open {
            open = self
                .changed
                .wait(open)
                .expect("gate lock must not be poisoned");
        }
    }

    fn open(&self) {
        *self.open.lock().expect("gate lock must not be poisoned") = true;
        self.changed.notify_all();
    }
}

/// Releases a blocked actor before `ThreadRuntime` joins after an assertion failure.
struct GateRelease<'a>(&'a Gate);

impl Drop for GateRelease<'_> {
    fn drop(&mut self) {
        self.0.open();
    }
}

fn poll_pending<F: Future>(future: Pin<&mut F>) {
    let waker = futures_util::task::noop_waker();
    let mut context = Context::from_waker(&waker);
    assert!(matches!(future.poll(&mut context), Poll::Pending));
}

fn assert_unpin<T: Unpin>() {}

#[test]
fn diagnostics_observation_hides_values_without_reinvoking_the_selector() {
    struct Secret(String);

    let runtime = ThreadRuntime::new();
    let store = Store::new(0, Counter, runtime.runtime());
    let calls = Arc::new(AtomicUsize::new(0));
    let selector_calls = Arc::clone(&calls);
    futures_executor::block_on(async {
        let mut values = store
            .observe(move |state| {
                selector_calls.fetch_add(1, Ordering::SeqCst);
                Secret(format!("private-value-{state}"))
            })
            .await
            .unwrap();
        let initial = format!("{values:?}");
        assert!(initial.starts_with("Observation {"));
        assert!(initial.contains("delivered_version: 0"));
        assert!(initial.contains("active: true"));
        assert!(!initial.contains("private-value"));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(values.next().await.unwrap().0, "private-value-0");

        let mut next = Box::pin(values.next());
        poll_pending(next.as_mut());
        drop(next);
        assert_eq!(format!("{values:?}"), initial);
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        store.send(()).await.unwrap().finish().await;
        assert_eq!(values.next().await.unwrap().0, "private-value-1");
        let delivered = format!("{values:?}");
        assert!(delivered.contains("delivered_version: 1"));
        assert!(delivered.contains("active: true"));
        assert!(!delivered.contains("private-value"));
        assert_eq!(calls.load(Ordering::SeqCst), 2);

        store.shutdown().await.unwrap();
        assert!(values.next().await.is_none());
        let closed = format!("{values:?}");
        assert!(closed.contains("delivered_version: 1"));
        assert!(closed.contains("active: false"));
        assert!(!closed.contains("private-value"));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    });
    runtime.join_all();
}

// An unpolled actor must release distinct registration at either ingress stage.
#[test]
fn distinct_constructor_stops_before_and_after_independent_admission() {
    struct CountingReducer(Arc<AtomicUsize>);
    impl Reducer for CountingReducer {
        type State = usize;
        type Action = u8;

        fn reduce(&mut self, _: &mut usize, _: &u8, _: &mut ReduceContext<'_>) -> Effect<u8> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Effect::none()
        }
    }

    struct SelectorCapture(Arc<AtomicUsize>);
    impl Drop for SelectorCapture {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    for before_admission in [true, false] {
        let runtime = manual_support::ManualRuntime::new();
        let reductions = Arc::new(AtomicUsize::new(0));
        let store = Store::builder(
            0,
            CountingReducer(Arc::clone(&reductions)),
            runtime.runtime(),
        )
        .mailbox_capacity(1)
        .build()
        .unwrap();
        let actor = runtime.take_only_queued_task();
        let mut context = Context::from_waker(std::task::Waker::noop());
        let filler = before_admission.then(|| store.try_send(11).unwrap());
        if before_admission {
            let error = store.try_send(22).unwrap_err();
            assert!(error.is_full());
            assert_eq!(error.into_action(), 22);
        }
        let calls = Arc::new(AtomicUsize::new(0));
        let drops = Arc::new(AtomicUsize::new(0));
        let capture = SelectorCapture(Arc::clone(&drops));
        let selector_calls = Arc::clone(&calls);
        let mut registration = Box::pin(store.observe_distinct(move |state| {
            let _retained_capture = &capture;
            selector_calls.fetch_add(1, Ordering::SeqCst);
            *state
        }));
        assert!(registration.as_mut().poll(&mut context).is_pending());
        // With no actor poll or other producer, only filler/registration can own the slot.
        let error = store.try_send(33).unwrap_err();
        assert!(error.is_full());
        assert_eq!(error.into_action(), 33);
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        drop(actor);
        assert_eq!(drops.load(Ordering::SeqCst), usize::from(!before_admission));
        assert!(matches!(
            registration.as_mut().poll(&mut context),
            Poll::Ready(Err(rca_core::StoreStopped { .. }))
        ));
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(reductions.load(Ordering::SeqCst), 0);
        let mut closed = std::pin::pin!(store.closed());
        assert!(matches!(
            closed.as_mut().poll(&mut context),
            Poll::Ready(())
        ));
        if let Some(filler) = filler {
            let mut receipt = std::pin::pin!(filler.finish());
            assert!(matches!(
                receipt.as_mut().poll(&mut context),
                Poll::Ready(Err(_))
            ));
        }
        drop(registration);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }
}

#[test]
fn observation_moves_initial_and_committed_values_without_clone() {
    let runtime = ThreadRuntime::new();
    let store = Store::new(0, Counter, runtime.runtime());

    futures_executor::block_on(async {
        let mut values = store
            .observe(|state| MoveOnly(*state))
            .await
            .expect("registration");

        assert_eq!(values.next().await.map(|value| value.0), Some(0));
        store.send(()).await.expect("first commit").finish().await;
        assert_eq!(values.next().await.map(|value| value.0), Some(1));
        store.shutdown().await.expect("clean shutdown");
        assert!(values.next().await.is_none());
    });
    runtime.join_all();
}

#[test]
fn slow_observer_receives_latest_value() {
    let runtime = ThreadRuntime::new();
    let store = Store::new(0, Counter, runtime.runtime());

    futures_executor::block_on(async {
        let mut values = store.observe(|state| *state).await.expect("registration");
        assert_eq!(values.next().await, Some(0));
        for _ in 0..100 {
            store.send(()).await.expect("commit").finish().await;
        }
        assert_eq!(values.next().await, Some(100));
        store.shutdown().await.expect("clean shutdown");
    });
    runtime.join_all();
}

#[test]
fn slow_distinct_observer_receives_latest_value() {
    let runtime = ThreadRuntime::new();
    let store = Store::new(0, Counter, runtime.runtime());

    futures_executor::block_on(async {
        let mut values = store
            .observe_distinct(|state| *state)
            .await
            .expect("registration");
        assert_eq!(values.next().await, Some(0));
        for _ in 0..100 {
            store.send(()).await.expect("commit").finish().await;
        }
        assert_eq!(values.next().await, Some(100));
        store.shutdown().await.expect("clean shutdown");
    });
    runtime.join_all();
}

#[test]
fn distinct_observation_clones_once_per_published_value() {
    let runtime = ThreadRuntime::new();
    let store = Store::new(0, Counter, runtime.runtime());
    let clones = Arc::new(AtomicUsize::new(0));
    let selector_clones = Arc::clone(&clones);

    futures_executor::block_on(async {
        let mut values = store
            .observe_distinct(move |state| CloneProbe {
                value: *state / 2,
                clones: Arc::clone(&selector_clones),
            })
            .await
            .expect("registration");
        assert_eq!(clones.load(Ordering::Acquire), 1);
        assert_eq!(values.next().await.expect("initial").value, 0);

        store.send(()).await.expect("equal commit").finish().await;
        assert_eq!(clones.load(Ordering::Acquire), 1);
        let mut equal = Box::pin(values.next());
        poll_pending(equal.as_mut());
        drop(equal);

        store.send(()).await.expect("unequal commit").finish().await;
        assert_eq!(clones.load(Ordering::Acquire), 2);
        assert_eq!(values.next().await.expect("changed value").value, 1);
        store.shutdown().await.expect("clean shutdown");
    });
    runtime.join_all();
}

#[test]
fn cancelled_next_resumes_same_take() {
    let runtime = ThreadRuntime::new();
    let store = Store::new(0, Counter, runtime.runtime());

    futures_executor::block_on(async {
        let mut values = store.observe(|state| *state).await.expect("registration");
        assert_eq!(values.next().await, Some(0));
        let mut pending = Box::pin(values.next());
        let waker = futures_util::task::noop_waker();
        let mut context = Context::from_waker(&waker);
        assert!(matches!(pending.as_mut().poll(&mut context), Poll::Pending));
        drop(pending);

        store.send(()).await.expect("commit").finish().await;
        assert_eq!(values.next().await, Some(1));
        store.shutdown().await.expect("clean shutdown");
    });
    runtime.join_all();
}

#[test]
fn selector_can_try_send_without_reentrant_reduction() {
    let runtime = ThreadRuntime::new();
    let store = Store::new(0, Counter, runtime.runtime());
    let once = Arc::new(AtomicBool::new(false));
    let selector_store = store.clone();
    let selector_once = Arc::clone(&once);

    futures_executor::block_on(async {
        let mut values = store
            .observe(move |state| {
                if !selector_once.swap(true, Ordering::AcqRel) {
                    drop(selector_store.try_send(()).expect("bounded admission"));
                }
                *state
            })
            .await
            .expect("registration");

        assert_eq!(values.next().await, Some(0));
        assert_eq!(values.next().await, Some(1));
        store.shutdown().await.expect("clean shutdown");
    });
    runtime.join_all();
}

#[test]
fn task_update_notifies_only_the_changed_keyed_row() {
    struct ModifyRow;
    impl rca_core::Feature for ModifyRow {
        type State = rca_core::KeyedState<usize, usize>;
        type Action = usize;

        fn reduce(
            &mut self,
            _: &mut Self::State,
            action: &usize,
            context: &mut rca_core::FeatureContext<'_, Self::State, usize>,
        ) {
            let row = *action;
            context.task(rca_core::Task::<Self::State, usize>::run(
                move |context| async move {
                    context
                        .update(move |rows| *rows.get_mut(&row).unwrap() += 1)
                        .await
                        .unwrap();
                },
            ));
        }
    }

    for count in [1, 16, 1024] {
        let runtime = ThreadRuntime::new();
        let rows = (0..count).map(|id| (id, 0)).collect();
        let store = Store::feature_builder(rows, ModifyRow, runtime.runtime())
            .build()
            .unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        futures_executor::block_on(async {
            let mut observations = Vec::with_capacity(count);
            for id in 0..count {
                let calls = Arc::clone(&calls);
                let mut values = store
                    .observe_distinct(move |rows| {
                        calls.fetch_add(1, Ordering::SeqCst);
                        *rows.get(&id).unwrap()
                    })
                    .await
                    .unwrap();
                assert_eq!(values.next().await, Some(0));
                observations.push(values);
            }
            let changed = count / 2;
            store.send(changed).await.unwrap().finish().await;
            for (id, values) in observations.iter_mut().enumerate() {
                if id == changed {
                    assert_eq!(values.next().await, Some(1));
                }
                poll_pending(Box::pin(values.next()).as_mut());
            }
            // Generic selectors can read any root state. Distinctness avoids
            // delivery, not selection: registration, action, then task update.
            assert_eq!(calls.load(Ordering::SeqCst), count * 3);
            store.shutdown().await.unwrap();
            for values in &mut observations {
                assert!(values.next().await.is_none());
            }
        });
        runtime.join_all();
    }
}

#[test]
fn cancelled_queued_take_resumes_without_repeating_value() {
    struct SendOnly(std::cell::Cell<usize>);

    fn require_send<T: Send>(value: T) -> T {
        value
    }

    let runtime = ThreadRuntime::new();
    let store = Store::new(0, Counter, runtime.runtime());

    futures_executor::block_on(async {
        let mut values = require_send(
            store
                .observe(|state| SendOnly(std::cell::Cell::new(*state)))
                .await
                .expect("registration"),
        );
        assert_eq!(values.next().await.expect("initial value").0.get(), 0);
        store.send(()).await.expect("commit").finish().await;

        let gate = Arc::new(Gate::default());
        let release = GateRelease(gate.as_ref());
        let reader_gate = Arc::clone(&gate);
        let (started, started_receive) = std::sync::mpsc::sync_channel(1);
        let mut blocked_read = Box::pin(store.read(move |_| {
            started.send(()).expect("test receives read start");
            reader_gate.wait();
        }));
        poll_pending(blocked_read.as_mut());
        started_receive
            .recv_timeout(Duration::from_secs(10))
            .expect("blocking read must start");

        let mut pending_take = Box::pin(require_send(values.next()));
        poll_pending(pending_take.as_mut());
        drop(pending_take);
        let mut marker = Box::pin(store.read(|_| ()));
        poll_pending(marker.as_mut());
        drop(release);
        blocked_read.await.expect("blocking read completes");
        marker.await.expect("take command drained before marker");

        assert_eq!(values.next().await.expect("queued value").0.get(), 1);
        let mut next = Box::pin(values.next());
        poll_pending(next.as_mut());
        drop(next);
        store.send(()).await.expect("next commit").finish().await;
        assert_eq!(values.next().await.expect("next value").0.get(), 2);
        store.shutdown().await.expect("clean shutdown");
    });
    runtime.join_all();
}

#[test]
fn cancelled_full_mailbox_take_resumes_after_ordered_update() {
    let runtime = ThreadRuntime::new();
    let store = Store::builder(0, Counter, runtime.runtime())
        .mailbox_capacity(1)
        .build()
        .expect("valid store");

    futures_executor::block_on(async {
        let mut values = store.observe(|state| MoveOnly(*state)).await.unwrap();
        assert_eq!(values.next().await.unwrap().0, 0);
        store.send(()).await.unwrap().finish().await;

        let gate = Arc::new(Gate::default());
        let release = GateRelease(gate.as_ref());
        let reader_gate = Arc::clone(&gate);
        let (started, started_receive) = std::sync::mpsc::sync_channel(1);
        let mut blocked_read = Box::pin(store.read(move |_| {
            started.send(()).unwrap();
            reader_gate.wait();
        }));
        poll_pending(blocked_read.as_mut());
        started_receive
            .recv_timeout(Duration::from_secs(10))
            .expect("blocking read must start");
        let queued_update = store.try_send(()).expect("fill the bounded mailbox");
        assert!(store.try_send(()).unwrap_err().is_full());

        let mut pending_take = Box::pin(values.next());
        let was_pending = pending_take
            .as_mut()
            .poll(&mut Context::from_waker(std::task::Waker::noop()))
            .is_pending();
        drop(pending_take);
        drop(release);
        assert!(was_pending, "full-mailbox take must wait for admission");
        blocked_read.await.unwrap();
        queued_update.finish().await.unwrap().finish().await;

        // Cancellation of next() must preserve a take waiting for admission,
        // and extraction must remain ordered after the already-queued update.
        assert_eq!(values.next().await.expect("resumed take").0, 2);
        let mut next = Box::pin(values.next());
        poll_pending(next.as_mut());
        drop(next);
        store.send(()).await.unwrap().finish().await;
        assert_eq!(values.next().await.unwrap().0, 3);
        store.shutdown().await.unwrap();
    });
    runtime.join_all();
}

#[test]
fn notification_and_take_registration_do_not_lose_commits() {
    let runtime = ThreadRuntime::new();
    let store = Store::new(0, Counter, runtime.runtime());

    futures_executor::block_on(async {
        let mut values = store.observe(|state| *state).await.expect("registration");
        assert_eq!(values.next().await, Some(0));
        for expected in 1..=1_000 {
            let commit = store.send(());
            let next = values.next();
            let (task, value) = futures_util::future::join(commit, next).await;
            task.expect("commit").finish().await;
            assert_eq!(value, Some(expected));
        }
        store.shutdown().await.expect("clean shutdown");
    });
    runtime.join_all();
}

#[test]
fn observation_retains_at_most_delivered_and_latest_values() {
    let runtime = ThreadRuntime::new();
    let store = Store::new(0, Counter, runtime.runtime());
    let live = Arc::new(AtomicUsize::new(0));
    let drops = Arc::new(AtomicUsize::new(0));
    let selector_live = Arc::clone(&live);
    let selector_drops = Arc::clone(&drops);

    futures_executor::block_on(async {
        let mut values = store
            .observe(move |state| DropProbe::new(*state, &selector_live, &selector_drops))
            .await
            .expect("registration");
        assert_eq!(live.load(Ordering::Acquire), 1);

        store.send(()).await.expect("first commit").finish().await;
        assert_eq!(live.load(Ordering::Acquire), 2);
        store.send(()).await.expect("second commit").finish().await;
        assert_eq!(live.load(Ordering::Acquire), 2);

        let initial = values.next().await.expect("initial value");
        assert_eq!(initial.value, 0);
        drop(initial);
        assert_eq!(live.load(Ordering::Acquire), 1);

        let delivered = values.next().await.expect("latest value");
        assert_eq!(delivered.value, 2);
        store.send(()).await.expect("third commit").finish().await;
        store.send(()).await.expect("fourth commit").finish().await;
        assert_eq!(live.load(Ordering::Acquire), 2);

        drop(delivered);
        drop(values);
        store.send(()).await.expect("cleanup commit").finish().await;
        assert_eq!(live.load(Ordering::Acquire), 0);
        assert_eq!(drops.load(Ordering::Acquire), 5);
        store.shutdown().await.expect("clean shutdown");
    });
    runtime.join_all();
}

#[test]
fn cancelled_registration_drops_late_initial_value() {
    let runtime = ThreadRuntime::new();
    let store = Store::new(0, Counter, runtime.runtime());
    let gate = Arc::new(Gate::default());
    let release = GateRelease(gate.as_ref());
    let selector_gate = Arc::clone(&gate);
    let (started, started_receive) = std::sync::mpsc::sync_channel(1);
    let live = Arc::new(AtomicUsize::new(0));
    let drops = Arc::new(AtomicUsize::new(0));
    let selector_live = Arc::clone(&live);
    let selector_drops = Arc::clone(&drops);

    let mut registration = Box::pin(store.observe(move |state| {
        started.send(()).expect("test receives selector start");
        selector_gate.wait();
        DropProbe::new(*state, &selector_live, &selector_drops)
    }));
    poll_pending(registration.as_mut());
    started_receive
        .recv_timeout(Duration::from_secs(10))
        .expect("selector must start after admission");
    drop(registration);
    drop(release);

    futures_executor::block_on(async {
        store.read(|_| ()).await.expect("registration drained");
        assert_eq!(live.load(Ordering::Acquire), 0);
        assert_eq!(drops.load(Ordering::Acquire), 1);
        store.shutdown().await.expect("clean shutdown");
    });
    runtime.join_all();
}

#[test]
fn cancelled_registration_drops_delivered_initial_value_once() {
    let runtime = ThreadRuntime::new();
    let store = Store::new(0, Counter, runtime.runtime());
    let gate = Arc::new(Gate::default());
    let release = GateRelease(gate.as_ref());
    let selector_gate = Arc::clone(&gate);
    let calls = Arc::new(AtomicUsize::new(0));
    let selector_calls = Arc::clone(&calls);
    let live = Arc::new(AtomicUsize::new(0));
    let drops = Arc::new(AtomicUsize::new(0));
    let selector_live = Arc::clone(&live);
    let selector_drops = Arc::clone(&drops);

    let mut registration = Box::pin(store.observe(move |state| {
        selector_calls.fetch_add(1, Ordering::AcqRel);
        selector_gate.wait();
        DropProbe::new(*state, &selector_live, &selector_drops)
    }));
    poll_pending(registration.as_mut());
    drop(release);

    futures_executor::block_on(async {
        // The ordered read completes after the actor has sent the initial reply.
        // Leave the registration future unpolled so it still owns that reply.
        store.read(|_| ()).await.expect("registration replied");
        assert_eq!(calls.load(Ordering::Acquire), 1);
        assert_eq!(live.load(Ordering::Acquire), 1);
        assert_eq!(drops.load(Ordering::Acquire), 0);
        drop(registration);
        assert_eq!(live.load(Ordering::Acquire), 0);
        assert_eq!(drops.load(Ordering::Acquire), 1);

        store.read(|_| ()).await.expect("cancellation drained");
        store.send(()).await.expect("later commit").finish().await;
        assert_eq!(calls.load(Ordering::Acquire), 1);
        assert_eq!(live.load(Ordering::Acquire), 0);
        assert_eq!(drops.load(Ordering::Acquire), 1);
        store.shutdown().await.expect("clean shutdown");
    });
    runtime.join_all();
}

// Catches publishing to an observation dropped by the same action's reducer.
#[test]
fn dropping_observation_during_reduction_skips_commit_publication() {
    let runtime = ThreadRuntime::new();
    let observation = Arc::new(Mutex::new(None::<rca_core::Observation<DropProbe>>));
    let retained = Arc::clone(&observation);
    let store = Store::new(
        0,
        rca_core::Reduce::new(move |state: &mut usize, remove: &bool, _| {
            *state += 1;
            if *remove {
                let values = retained.lock().unwrap().take().unwrap();
                drop(values);
            }
            Effect::none()
        }),
        runtime.runtime(),
    );
    let calls = Arc::new(AtomicUsize::new(0));
    let selected = Arc::clone(&calls);
    let live = Arc::new(AtomicUsize::new(0));
    let drops = Arc::new(AtomicUsize::new(0));
    let selector_live = Arc::clone(&live);
    let selector_drops = Arc::clone(&drops);

    futures_executor::block_on(async {
        let mut values = store
            .observe(move |state| {
                selected.fetch_add(1, Ordering::SeqCst);
                DropProbe::new(*state, &selector_live, &selector_drops)
            })
            .await
            .unwrap();
        let initial = values.next().await.unwrap();
        assert_eq!(initial.value, 0);
        drop(initial);
        *observation.lock().unwrap() = Some(values);

        store.send(false).await.unwrap().finish().await;
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(Arc::strong_count(&calls), 2);
        assert_eq!(live.load(Ordering::SeqCst), 1);
        assert_eq!(drops.load(Ordering::SeqCst), 1);

        // Reduction drops the real observation before this commit publishes.
        store.send(true).await.unwrap().finish().await;
        assert!(observation.lock().unwrap().is_none());
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(Arc::strong_count(&calls), 1);
        assert_eq!(live.load(Ordering::SeqCst), 0);
        assert_eq!(drops.load(Ordering::SeqCst), 2);

        store.send(false).await.unwrap().finish().await;
        assert_eq!(store.read(|state| *state).await.unwrap(), 3);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        assert_eq!(drops.load(Ordering::SeqCst), 2);
        store.shutdown().await.unwrap();
    });
    runtime.join_all();
}

#[test]
fn dropping_observation_drops_queued_take_value_once() {
    let runtime = ThreadRuntime::new();
    let store = Store::new(0, Counter, runtime.runtime());
    let live = Arc::new(AtomicUsize::new(0));
    let drops = Arc::new(AtomicUsize::new(0));
    let selector_live = Arc::clone(&live);
    let selector_drops = Arc::clone(&drops);

    futures_executor::block_on(async {
        let mut values = store
            .observe(move |state| DropProbe::new(*state, &selector_live, &selector_drops))
            .await
            .expect("registration");
        drop(values.next().await.expect("initial value"));
        store.send(()).await.expect("commit").finish().await;

        let gate = Arc::new(Gate::default());
        let release = GateRelease(gate.as_ref());
        let reader_gate = Arc::clone(&gate);
        let (started, started_receive) = std::sync::mpsc::sync_channel(1);
        let mut blocked_read = Box::pin(store.read(move |_| {
            started.send(()).expect("test receives read start");
            reader_gate.wait();
        }));
        poll_pending(blocked_read.as_mut());
        started_receive
            .recv_timeout(Duration::from_secs(10))
            .expect("blocking read must start");

        let mut pending_take = Box::pin(values.next());
        poll_pending(pending_take.as_mut());
        let mut marker = Box::pin(store.read(|_| ()));
        poll_pending(marker.as_mut());
        drop(release);
        blocked_read.await.expect("blocking read completes");
        marker.await.expect("take command drained before marker");

        drop(pending_take);
        drop(values);
        assert_eq!(live.load(Ordering::Acquire), 0);
        assert_eq!(drops.load(Ordering::Acquire), 2);
        store.shutdown().await.expect("clean shutdown");
    });
    runtime.join_all();
}

#[test]
fn dropping_observation_before_queued_take_releases_value_once() {
    let runtime = ThreadRuntime::new();
    let store = Store::new(0, Counter, runtime.runtime());
    let live = Arc::new(AtomicUsize::new(0));
    let drops = Arc::new(AtomicUsize::new(0));
    let selector_live = Arc::clone(&live);
    let selector_drops = Arc::clone(&drops);

    futures_executor::block_on(async {
        let mut values = store
            .observe(move |state| DropProbe::new(*state, &selector_live, &selector_drops))
            .await
            .expect("registration");
        drop(values.next().await.expect("initial value"));
        store.send(()).await.expect("commit").finish().await;

        let gate = Arc::new(Gate::default());
        let release = GateRelease(gate.as_ref());
        let reader_gate = Arc::clone(&gate);
        let (started, started_receive) = std::sync::mpsc::sync_channel(1);
        let mut blocked_read = Box::pin(store.read(move |_| {
            started.send(()).expect("test receives read start");
            reader_gate.wait();
        }));
        poll_pending(blocked_read.as_mut());
        started_receive
            .recv_timeout(Duration::from_secs(10))
            .expect("blocking read must start");

        let mut pending_take = Box::pin(values.next());
        poll_pending(pending_take.as_mut());
        drop(pending_take);
        drop(values);
        let mut marker = Box::pin(store.read(|_| ()));
        poll_pending(marker.as_mut());
        drop(release);
        blocked_read.await.expect("blocking read completes");
        marker.await.expect("take command drained before marker");

        assert_eq!(live.load(Ordering::Acquire), 0);
        assert_eq!(drops.load(Ordering::Acquire), 2);
        store.shutdown().await.expect("clean shutdown");
        assert_eq!(drops.load(Ordering::Acquire), 2);
    });
    runtime.join_all();
}

#[test]
fn moved_waiting_observation_uses_new_waker_for_publication_and_closure() {
    struct WakeCount(AtomicUsize);

    impl std::task::Wake for WakeCount {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    let runtime = manual_support::ManualRuntime::new();
    let store = Store::new(0, Counter, runtime.runtime());
    let mut actor = runtime.take_only_queued_task();
    let mut actor_context = Context::from_waker(std::task::Waker::noop());
    let mut registration = Box::pin(store.observe(|state| std::cell::Cell::new(*state)));
    assert!(registration.as_mut().poll(&mut actor_context).is_pending());
    assert!(actor.as_mut().poll(&mut actor_context).is_pending());
    let Poll::Ready(values) = registration.as_mut().poll(&mut actor_context) else {
        panic!("registration must finish");
    };
    drop(registration);
    let mut values = values.unwrap();
    assert_eq!(futures_executor::block_on(values.next()).unwrap().get(), 0);

    let first = Arc::new(WakeCount(AtomicUsize::new(0)));
    let first_waker = std::task::Waker::from(Arc::clone(&first));
    let mut next = Box::pin(values.next());
    assert!(
        next.as_mut()
            .poll(&mut Context::from_waker(&first_waker))
            .is_pending()
    );
    drop(next);

    // Move the Observation itself after its listener has registered a waker.
    let mut moved = Box::new(values);
    let second = Arc::new(WakeCount(AtomicUsize::new(0)));
    let second_waker = std::task::Waker::from(Arc::clone(&second));
    let mut second_context = Context::from_waker(&second_waker);
    let mut next = Box::pin(moved.next());
    assert!(next.as_mut().poll(&mut second_context).is_pending());
    let before_publication = second.0.load(Ordering::SeqCst);
    let receipt = store.try_send(()).unwrap();
    assert!(actor.as_mut().poll(&mut actor_context).is_pending());
    assert!(second.0.load(Ordering::SeqCst) > before_publication);
    drop(futures_executor::block_on(receipt.finish()).unwrap());
    assert!(next.as_mut().poll(&mut second_context).is_pending());
    assert!(actor.as_mut().poll(&mut actor_context).is_pending());
    let Poll::Ready(Some(value)) = next.as_mut().poll(&mut second_context) else {
        panic!("moved observation must deliver its latest value");
    };
    assert_eq!(value.get(), 1);
    drop(next);

    let mut next = Box::pin(moved.next());
    assert!(next.as_mut().poll(&mut second_context).is_pending());
    let before_closure = second.0.load(Ordering::SeqCst);
    drop(store);
    assert!(actor.as_mut().poll(&mut actor_context).is_ready());
    assert!(second.0.load(Ordering::SeqCst) > before_closure);
    assert!(matches!(
        next.as_mut().poll(&mut second_context),
        Poll::Ready(None)
    ));
}

#[test]
fn final_owner_drop_preserves_only_already_written_take_replies() {
    for (reply_written, dispose_actor, consume_reply) in [
        (false, false, true),
        (true, false, true),
        (true, false, false),
        (false, true, true),
        (true, true, true),
        (true, true, false),
    ] {
        let runtime = manual_support::ManualRuntime::new();
        let store = Store::new(0, Counter, runtime.runtime());
        let mut actor = runtime.take_only_queued_task();
        let mut context = Context::from_waker(std::task::Waker::noop());
        let live = Arc::new(AtomicUsize::new(0));
        let drops = Arc::new(AtomicUsize::new(0));
        let selector_live = Arc::clone(&live);
        let selector_drops = Arc::clone(&drops);
        let mut values = {
            let registration =
                store.observe(move |state| DropProbe::new(*state, &selector_live, &selector_drops));
            let mut registration = std::pin::pin!(registration);
            assert!(registration.as_mut().poll(&mut context).is_pending());
            assert!(actor.as_mut().poll(&mut context).is_pending());
            let Poll::Ready(values) = registration.as_mut().poll(&mut context) else {
                panic!("registration must finish");
            };
            values.unwrap()
        };
        let initial = futures_executor::block_on(values.next()).unwrap();
        assert_eq!(initial.value, 0);
        drop(initial);
        {
            let update = store.send(());
            let mut update = std::pin::pin!(update);
            assert!(update.as_mut().poll(&mut context).is_pending());
            assert!(actor.as_mut().poll(&mut context).is_pending());
            let Poll::Ready(task) = update.as_mut().poll(&mut context) else {
                panic!("update must commit");
            };
            drop(task.unwrap());
        };
        {
            let next = values.next();
            let mut next = std::pin::pin!(next);
            assert!(next.as_mut().poll(&mut context).is_pending());
        };
        if reply_written {
            assert!(actor.as_mut().poll(&mut context).is_pending());
        }
        let closed = store.closed();
        if dispose_actor {
            drop(actor);
            drop(store);
        } else {
            drop(store);
            assert!(actor.as_mut().poll(&mut context).is_ready());
        }
        futures_executor::block_on(closed);
        assert_eq!(live.load(Ordering::Acquire), usize::from(reply_written));

        if consume_reply {
            let value = futures_executor::block_on(values.next());
            assert_eq!(
                value.as_ref().map(|value| value.value),
                reply_written.then_some(1)
            );
            drop(value);
            assert!(futures_executor::block_on(values.next()).is_none());
        }
        drop(values);
        assert_eq!(live.load(Ordering::Acquire), 0);
        assert_eq!(drops.load(Ordering::Acquire), 2);
    }
}

#[test]
fn final_store_owner_drop_closes_pending_observation() {
    let runtime = ThreadRuntime::new();
    let store = Store::new(0, Counter, runtime.runtime());
    let mut values =
        futures_executor::block_on(store.observe(|state| *state)).expect("registration");
    assert_eq!(futures_executor::block_on(values.next()), Some(0));

    let mut pending = Box::pin(values.next());
    poll_pending(pending.as_mut());
    drop(store);
    assert_eq!(futures_executor::block_on(pending.as_mut()), None);
    drop(pending);
    drop(values);
    runtime.join_all();
}

#[test]
fn dropping_observation_releases_selector_store_capture() {
    let runtime = ThreadRuntime::new();
    let store = Store::new(0, Counter, runtime.runtime());
    let closed = store.closed();
    let selector_store = store.clone();
    let values = futures_executor::block_on(store.observe(move |state| {
        std::hint::black_box(&selector_store);
        *state
    }))
    .expect("registration");

    drop(store);
    drop(values);
    futures_executor::block_on(closed);
    runtime.join_all();
}

#[test]
fn observation_is_unpin_for_pinned_values() {
    assert_unpin::<rca_core::Observation<std::marker::PhantomPinned>>();
}

#[cfg(feature = "futures-stream")]
#[test]
fn stream_delegates_to_inherent_take_state() {
    use futures_util::StreamExt;

    let runtime = ThreadRuntime::new();
    let store = Store::new(0, Counter, runtime.runtime());
    futures_executor::block_on(async {
        let mut values = store.observe(|state| *state).await.expect("registration");
        assert_eq!(StreamExt::next(&mut values).await, Some(0));
        store.shutdown().await.expect("clean shutdown");
        assert_eq!(StreamExt::next(&mut values).await, None);
    });
    runtime.join_all();
}

#[test]
fn full_mailbox_take_closes_when_actor_stops_before_admission() {
    use futures_util::FutureExt as _;

    let runtime = manual_support::ManualRuntime::new();
    let store = Store::builder(0, Counter, runtime.runtime())
        .mailbox_capacity(1)
        .build()
        .unwrap();
    let mut actor = runtime.take_only_queued_task();
    let mut context = Context::from_waker(std::task::Waker::noop());
    let live = Arc::new(AtomicUsize::new(0));
    let drops = Arc::new(AtomicUsize::new(0));
    let selector_live = Arc::clone(&live);
    let selector_drops = Arc::clone(&drops);
    let mut values = {
        let registration =
            store.observe(move |state| DropProbe::new(*state, &selector_live, &selector_drops));
        let mut registration = std::pin::pin!(registration);
        assert!(registration.as_mut().poll(&mut context).is_pending());
        assert!(actor.as_mut().poll(&mut context).is_pending());
        let Poll::Ready(values) = registration.as_mut().poll(&mut context) else {
            panic!("registration must finish");
        };
        values.unwrap()
    };
    let initial = values
        .next()
        .now_or_never()
        .expect("registration provides the initial value without actor progress")
        .unwrap();
    assert_eq!(initial.value, 0);
    drop(initial);

    let update = store.try_send(()).unwrap();
    assert!(actor.as_mut().poll(&mut context).is_pending());
    let committed = update
        .finish()
        .now_or_never()
        .expect("the preceding actor poll committed the update")
        .unwrap();
    committed
        .finish()
        .now_or_never()
        .expect("the update has no outstanding effects");
    assert_eq!(live.load(Ordering::Acquire), 1);

    let filler = store.try_send(()).unwrap();
    assert!(store.try_send(()).unwrap_err().is_full());
    let mut next = Box::pin(values.next());
    assert!(next.as_mut().poll(&mut context).is_pending());
    drop(actor);

    assert!(
        next.as_mut()
            .now_or_never()
            .expect("actor loss closes the pending take without further progress")
            .is_none()
    );
    drop(next);
    let _: rca_core::StoreStopped = filler
        .finish()
        .now_or_never()
        .expect("actor loss rejects the queued action without further progress")
        .expect_err("an action cannot commit after actor loss");
    assert!(
        values
            .next()
            .now_or_never()
            .expect("closed observation stays immediately ready")
            .is_none()
    );
    assert_eq!(live.load(Ordering::Acquire), 0);
    assert_eq!(drops.load(Ordering::Acquire), 2);
}

#[test]
fn backpressured_take_closes_when_actor_loses_reply_after_admission() {
    use futures_util::FutureExt as _;

    let runtime = manual_support::ManualRuntime::new();
    let store = Store::builder(0, Counter, runtime.runtime())
        .mailbox_capacity(1)
        .build()
        .unwrap();
    let mut actor = runtime.take_only_queued_task();
    let mut context = Context::from_waker(std::task::Waker::noop());
    let live = Arc::new(AtomicUsize::new(0));
    let drops = Arc::new(AtomicUsize::new(0));
    let selector_live = Arc::clone(&live);
    let selector_drops = Arc::clone(&drops);
    let mut values = {
        let registration =
            store.observe(move |state| DropProbe::new(*state, &selector_live, &selector_drops));
        let mut registration = std::pin::pin!(registration);
        assert!(registration.as_mut().poll(&mut context).is_pending());
        assert!(actor.as_mut().poll(&mut context).is_pending());
        let Poll::Ready(values) = registration.as_mut().poll(&mut context) else {
            panic!("registration must finish");
        };
        values.unwrap()
    };
    let initial = values.next().now_or_never().unwrap().unwrap();
    assert_eq!(initial.value, 0);
    drop(initial);
    let update = store.try_send(()).unwrap();
    assert!(actor.as_mut().poll(&mut context).is_pending());
    update
        .finish()
        .now_or_never()
        .unwrap()
        .unwrap()
        .finish()
        .now_or_never()
        .expect("no effects remain");
    assert_eq!(live.load(Ordering::Acquire), 1);

    let filler = store.try_send(()).unwrap();
    assert!(store.try_send(()).unwrap_err().is_full());
    let mut next = Box::pin(values.next());
    assert!(next.as_mut().poll(&mut context).is_pending());
    // Drain the filler without polling the deferred take: its send is still waiting.
    assert!(actor.as_mut().poll(&mut context).is_pending());
    filler
        .finish()
        .now_or_never()
        .expect("filler committed")
        .unwrap()
        .finish()
        .now_or_never()
        .expect("no effects remain");
    assert_eq!(
        drops.load(Ordering::Acquire),
        2,
        "initial and superseded values dropped"
    );
    // Resume that same deferred future so its send succeeds but its reply is pending.
    assert!(next.as_mut().poll(&mut context).is_pending());
    assert!(
        store.try_send(()).unwrap_err().is_full(),
        "the take now owns mailbox capacity"
    );
    drop(actor);

    assert!(
        next.as_mut()
            .now_or_never()
            .expect("reply loss wakes deferred take")
            .is_none()
    );
    drop(next);
    assert!(
        values
            .next()
            .now_or_never()
            .expect("closure is terminal")
            .is_none()
    );
    assert_eq!(live.load(Ordering::Acquire), 0);
    assert_eq!(drops.load(Ordering::Acquire), 3);
    assert_eq!(Arc::strong_count(&live), 1, "selector capture released");
    assert_eq!(Arc::strong_count(&drops), 1, "selector capture released");
    assert!(store.is_closed());
}
