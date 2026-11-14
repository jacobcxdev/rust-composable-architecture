//! Checks error classification and rendering from actual rejected Store operations.
//!
//! Backtrace checks compare rendering with the error's own backtrace. They do not
//! require backtrace capture to be enabled in the test environment.

#[expect(dead_code, reason = "error tests manually poll captured runtime tasks")]
mod support;

use std::error::Error;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::task::{Context, Poll, Waker};

use futures_executor::block_on;
use rca_core::{Effect, EffectSendError, EffectSender, ReduceContext, Reducer, Store};

/// Starts one sender export, then provides an input that must never be reduced.
enum Action {
    Start,
    Value,
}

/// Exports a real effect sender while its causal task remains pending.
struct Feature {
    /// Consumed by the single start action, before the effect waits indefinitely.
    export: Option<mpsc::Sender<EffectSender<Action>>>,
    /// Detects delivery after cancellation or Store closure.
    reductions: Arc<AtomicUsize>,
}

impl Reducer for Feature {
    type State = ();
    type Action = Action;

    fn reduce(
        &mut self,
        (): &mut (),
        action: &Action,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        match action {
            Action::Start => {
                let export = self.export.take().expect("one effect starts");
                Effect::run(move |sender| async move {
                    export.send(sender).expect("test retains receiver");
                    std::future::pending::<()>().await;
                })
            }
            Action::Value => {
                self.reductions.fetch_add(1, Ordering::Relaxed);
                Effect::none()
            }
        }
    }
}

/// Ends delivery authority and requires rejection without further actor progress.
///
/// Cancellation keeps the Store open. The other path drops its last owner.
fn rejected_effect_delivery(cancel: bool) -> EffectSendError {
    let runtime = support::ManualRuntime::new();
    let (export, receive) = mpsc::channel();
    let reductions = Arc::new(AtomicUsize::new(0));
    let store = Store::new(
        (),
        Feature {
            export: Some(export),
            reductions: Arc::clone(&reductions),
        },
        runtime.runtime(),
    );
    let mut actor = runtime.take_only_queued_task();
    let receipt = store.try_send(Action::Start).expect("start admitted");
    let mut context = Context::from_waker(Waker::noop());
    assert!(actor.as_mut().poll(&mut context).is_pending());
    let task = block_on(receipt.finish()).expect("start committed");
    let mut effect = runtime.take_only_queued_task();
    assert!(effect.as_mut().poll(&mut context).is_pending());
    let sender = receive.try_recv().expect("effect exported sender");

    if cancel {
        task.cancel();
        assert!(!store.is_closed());
    } else {
        drop(store);
    }
    let mut send = std::pin::pin!(sender.send(Action::Value));
    let Poll::Ready(Err(error)) = send.as_mut().poll(&mut context) else {
        panic!("ended delivery authority must reject without waiting for actor progress");
    };
    assert_eq!(reductions.load(Ordering::Relaxed), 0);
    error
}

#[test]
fn cancelled_delivery_diagnostics_do_not_report_store_closure() {
    // A swapped error branch would incorrectly tell callers their Store stopped.
    let error = rejected_effect_delivery(true);
    assert!(error.is_cancelled());
    assert!(!error.is_store_stopped());
    let display = error.to_string();
    assert_eq!(
        display.lines().next(),
        Some("effect action delivery was cancelled")
    );
    assert!(display.ends_with(&error.backtrace().to_string()));
    assert!(format!("{error:?}").contains("kind: Cancelled"));
    assert!(error.source().is_none());
}

#[test]
fn stopped_delivery_diagnostics_do_not_report_cancellation() {
    let error = rejected_effect_delivery(false);
    assert!(!error.is_cancelled());
    assert!(error.is_store_stopped());
    let display = error.to_string();
    assert_eq!(
        display.lines().next(),
        Some("store stopped before effect action delivery")
    );
    assert!(display.ends_with(&error.backtrace().to_string()));
    assert!(format!("{error:?}").contains("kind: StoreStopped"));
    assert!(error.source().is_none());
}

#[test]
fn actor_loss_reports_the_interrupted_read_and_shutdown_operations() {
    let runtime = support::ManualRuntime::new();
    let store = Store::new(
        (),
        Feature {
            export: None,
            reductions: Arc::new(AtomicUsize::new(0)),
        },
        runtime.runtime(),
    );
    let mut read = Box::pin(store.read(|()| ()));
    let mut shutdown = Box::pin(store.shutdown());
    let mut context = Context::from_waker(Waker::noop());
    assert!(read.as_mut().poll(&mut context).is_pending());
    assert!(shutdown.as_mut().poll(&mut context).is_pending());
    runtime.drop_queued();

    let read_error = block_on(read).expect_err("actor loss interrupts admitted read");
    assert_eq!(
        read_error.to_string().lines().next(),
        Some("store operation ended before successful completion")
    );
    assert!(
        read_error
            .to_string()
            .ends_with(&read_error.backtrace().to_string())
    );
    assert!(read_error.source().is_none());
    let shutdown_error = block_on(shutdown).expect_err("actor loss is not graceful shutdown");
    assert!(shutdown_error.is_store_stopped());
    assert_eq!(
        shutdown_error.to_string().lines().next(),
        Some("store stopped before graceful shutdown")
    );
    assert!(
        shutdown_error
            .to_string()
            .ends_with(&shutdown_error.backtrace().to_string())
    );
    assert!(format!("{shutdown_error:?}").starts_with("ShutdownError {"));
    assert!(shutdown_error.source().is_none());
    assert!(store.is_closed());
}
