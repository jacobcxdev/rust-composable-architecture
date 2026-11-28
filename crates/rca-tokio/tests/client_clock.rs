//! Checks public clock deadlines, first-poll readiness and cancellation ownership.

use std::task::{Context, Poll, Waker};
use std::time::Duration;

use rca_core::MonotonicInstant;
use rca_tokio::TokioRuntime;

#[tokio::test]
async fn elapsed_and_zero_public_waits_are_ready_on_first_poll() {
    let clock = TokioRuntime::current().unwrap().clock();
    for mut wait in [
        clock.sleep(Duration::ZERO),
        clock.sleep_until(clock.now()),
        clock.sleep_until(MonotonicInstant::ZERO),
    ] {
        assert_eq!(
            wait.as_mut().poll(&mut Context::from_waker(Waker::noop())),
            Poll::Ready(())
        );
    }
}

#[tokio::test(start_paused = true)]
async fn delayed_poll_keeps_invocation_deadline_and_max_waits_cancel_safely() {
    let runtime = TokioRuntime::current().unwrap();
    let clock = runtime.clock();
    let mut wait = clock.sleep(Duration::from_secs(10));
    tokio::time::advance(Duration::from_secs(10)).await;
    assert_eq!(clock.now(), runtime.now());
    assert_eq!(
        wait.as_mut().poll(&mut Context::from_waker(Waker::noop())),
        Poll::Ready(())
    );

    drop(clock.sleep(Duration::MAX));
    for mut wait in [
        clock.sleep(Duration::MAX),
        clock.sleep_until(MonotonicInstant::MAX),
    ] {
        assert_eq!(
            wait.as_mut().poll(&mut Context::from_waker(Waker::noop())),
            Poll::Pending
        );
        drop(wait);
    }
    clock.sleep(Duration::ZERO).await;
}

#[tokio::test]
async fn cancelling_a_registered_public_wait_releases_its_owned_capture() {
    let clock = TokioRuntime::current().unwrap().clock();
    let capture = std::sync::Arc::new(());
    let weak = std::sync::Arc::downgrade(&capture);
    let (registered, registration) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let mut wait = clock.sleep(Duration::MAX);
        std::future::poll_fn(|cx| {
            assert_eq!(wait.as_mut().poll(cx), Poll::Pending);
            Poll::Ready(())
        })
        .await;
        registered.send(()).unwrap();
        wait.await;
        drop(capture);
    });
    registration.await.unwrap();
    assert!(weak.upgrade().is_some());
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert!(weak.upgrade().is_none());
}
