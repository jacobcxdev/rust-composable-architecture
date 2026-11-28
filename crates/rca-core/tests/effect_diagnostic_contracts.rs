//! Checks that diagnostics neither expose private data nor execute deferred work.

#[expect(dead_code, reason = "diagnostic tests manually poll runtime tasks")]
mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Waker};
use std::time::Duration;

use futures_util::FutureExt;
use rca_core::{
    DependencyKey, Effect, EffectSender, MonotonicInstant, Reduce, ReduceContext, Reducer, Store,
};

/// Counts private payload destruction without implementing `Clone` or `Debug`.
struct PrivateAction {
    value: u8,
    secret: &'static str,
    drops: Arc<AtomicUsize>,
}

impl Drop for PrivateAction {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn reducer_and_context_diagnostics_preserve_private_captures_and_dependencies() {
    /// Distinguishes the installed private dependency from its public default.
    struct Endpoint;

    impl DependencyKey for Endpoint {
        type Value = &'static str;

        fn live_value() -> Self::Value {
            "public-default"
        }
    }

    let runtime = support::ManualRuntime::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let call_count = Arc::clone(&calls);
    let drops = Arc::new(AtomicUsize::new(0));
    let retained = PrivateAction {
        value: 7,
        secret: "sensitive-reducer-account-4821",
        drops: Arc::clone(&drops),
    };
    let reducer = Reduce::new(
        move |state: &mut u8, action: &u8, context: &mut ReduceContext<'_>| {
            let payload = &retained;
            let diagnostic = format!("{context:?}");
            assert!(diagnostic.starts_with("ReduceContext"));
            assert!(!diagnostic.contains("sensitive"));
            assert_eq!(*context.dependency::<Endpoint>(), "sensitive-endpoint-4821");
            assert_eq!(payload.secret, "sensitive-reducer-account-4821");
            *state += payload.value + action;
            call_count.fetch_add(1, Ordering::SeqCst);
            Effect::none()
        },
    );
    assert_eq!(format!("{reducer:?}"), "Reduce { .. }");
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(drops.load(Ordering::SeqCst), 0);

    let store = Store::builder(0, reducer, runtime.runtime())
        .with_dependency::<Endpoint>("sensitive-endpoint-4821")
        .build()
        .expect("valid dependencies");
    let mut actor = runtime.take_only_queued_task();
    let receipt = store.try_send(5).expect("admitted action");
    let mut cx = Context::from_waker(Waker::noop());
    assert!(actor.as_mut().poll(&mut cx).is_pending());
    assert!(
        receipt
            .finish()
            .now_or_never()
            .unwrap()
            .unwrap()
            .is_finished()
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(drops.load(Ordering::SeqCst), 0);

    let mut read = Box::pin(store.read(|state| *state));
    assert!(read.as_mut().poll(&mut cx).is_pending());
    assert!(actor.as_mut().poll(&mut cx).is_pending());
    assert!(matches!(read.now_or_never(), Some(Ok(12))));
    drop(store);
    assert!(actor.as_mut().poll(&mut cx).is_ready());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[test]
fn diagnostics_retain_unlaunched_payloads_without_exposing_them() {
    let drops = Arc::new(AtomicUsize::new(0));
    let payload = || PrivateAction {
        value: 7,
        secret: "sensitive-action-account-4821",
        drops: Arc::clone(&drops),
    };
    let effects = [
        (Effect::send(payload()), 1),
        (Effect::send_after(Duration::from_secs(1), payload()), 1),
        (Effect::send_at(MonotonicInstant::MAX, payload()), 1),
        (
            Effect::merge([Effect::send(payload()), Effect::send(payload())]),
            2,
        ),
        (Effect::send(payload()).without_inherited_cancellation(), 1),
        (
            Effect::send(payload()).cancellable("sensitive-cancellation-account-4821"),
            1,
        ),
        (Effect::cancel("sensitive-cancellation-account-4821"), 0),
    ];

    let mut released = 0;
    for (effect, retained_payloads) in effects {
        let diagnostic = format!("{effect:?}");
        assert!(diagnostic.starts_with("Effect::"));
        assert!(!diagnostic.contains("sensitive"));
        assert_eq!(drops.load(Ordering::SeqCst), released);
        drop(effect);
        released += retained_payloads;
        assert_eq!(drops.load(Ordering::SeqCst), released);
    }
    assert_eq!(drops.load(Ordering::SeqCst), 7);
}

#[test]
fn interval_diagnostics_retain_the_unlaunched_generator() {
    let generated = Arc::new(AtomicUsize::new(0));
    let generation_count = Arc::clone(&generated);
    let drops = Arc::new(AtomicUsize::new(0));
    let retained = PrivateAction {
        value: 7,
        secret: "sensitive-interval-account-4821",
        drops: Arc::clone(&drops),
    };
    let effect = Effect::interval(Duration::from_secs(1), move || {
        generation_count.fetch_add(1, Ordering::SeqCst);
        let payload = &retained;
        payload.value
    });

    let diagnostic = format!("{effect:?}");
    assert!(diagnostic.starts_with("Effect::"));
    assert!(!diagnostic.contains("sensitive-interval-account-4821"));
    assert_eq!(generated.load(Ordering::SeqCst), 0);
    assert_eq!(drops.load(Ordering::SeqCst), 0);

    drop(effect);
    assert_eq!(generated.load(Ordering::SeqCst), 0);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

/// Launches one mapped effect and records its expected terminal action.
struct Feature {
    effect: Option<Effect<u16>>,
    commits: Arc<AtomicUsize>,
}

impl Reducer for Feature {
    type State = ();
    type Action = u16;

    fn reduce(&mut self, (): &mut (), action: &u16, _: &mut ReduceContext<'_>) -> Effect<u16> {
        if *action == 0 {
            self.effect.take().expect("one launch")
        } else {
            assert_eq!(*action, 107);
            self.commits.fetch_add(1, Ordering::SeqCst);
            Effect::default().without_inherited_cancellation()
        }
    }
}

#[test]
fn logging_mapped_work_and_its_sender_preserves_delivery_and_completion() {
    let runtime = support::ManualRuntime::new();
    let factories = Arc::new(AtomicUsize::new(0));
    let mappings = Arc::new(AtomicUsize::new(0));
    let drops = Arc::new(AtomicUsize::new(0));
    let commits = Arc::new(AtomicUsize::new(0));
    let factory_count = Arc::clone(&factories);
    let sender_mapping_count = Arc::clone(&mappings);
    let mapping_count = Arc::clone(&mappings);
    let payload_drops = Arc::clone(&drops);
    let effect = Effect::run(move |sender: EffectSender<PrivateAction>| {
        factory_count.fetch_add(1, Ordering::SeqCst);
        let diagnostic = format!("{sender:?}");
        assert!(diagnostic.starts_with("EffectSender"));
        assert!(!diagnostic.contains("sensitive"));
        assert_eq!(sender_mapping_count.load(Ordering::SeqCst), 0);
        async move {
            sender
                .send(PrivateAction {
                    value: 7,
                    secret: "sensitive-action-account-4821",
                    drops: payload_drops,
                })
                .await
                .expect("diagnosed sender still commits its mapped action");
        }
    })
    .map(move |action| {
        mapping_count.fetch_add(1, Ordering::SeqCst);
        assert_eq!(action.secret, "sensitive-action-account-4821");
        u16::from(action.value) + 100
    });

    let diagnostic = format!("{effect:?}");
    assert!(diagnostic.starts_with("Effect::"));
    assert!(!diagnostic.contains("sensitive"));
    assert_eq!(factories.load(Ordering::SeqCst), 0);
    assert_eq!(mappings.load(Ordering::SeqCst), 0);
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    let store = Store::new(
        (),
        Feature {
            effect: Some(effect),
            commits: Arc::clone(&commits),
        },
        runtime.runtime(),
    );
    let mut actor = runtime.take_only_queued_task();
    let receipt = store.try_send(0).expect("start admission");
    let mut cx = Context::from_waker(Waker::noop());
    assert!(actor.as_mut().poll(&mut cx).is_pending());
    let task = receipt
        .finish()
        .now_or_never()
        .expect("start was reduced")
        .expect("start commit");
    let mut run = runtime.take_only_queued_task();
    assert!(!task.is_finished());
    assert!(run.as_mut().poll(&mut cx).is_pending());
    assert_eq!(factories.load(Ordering::SeqCst), 1);
    assert_eq!(mappings.load(Ordering::SeqCst), 1);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert_eq!(commits.load(Ordering::SeqCst), 0);
    assert!(actor.as_mut().poll(&mut cx).is_pending());
    assert_eq!(commits.load(Ordering::SeqCst), 1);
    assert!(run.as_mut().poll(&mut cx).is_ready());
    assert!(actor.as_mut().poll(&mut cx).is_pending());
    assert_eq!(mappings.load(Ordering::SeqCst), 1);
    assert!(task.is_finished());
    assert!(task.finish().now_or_never().is_some());
    // The default empty continuation must not launch a detached boundary task.
    assert_eq!(runtime.spawn_count(), 2);
}
