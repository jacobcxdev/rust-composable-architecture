#[path = "support/thread_runtime.rs"]
mod support;

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::task::{Context, Poll};

use rca_core::{Effect, ReduceContext, Reducer, Store};
use support::ThreadRuntime;

struct Counter(Arc<AtomicBool>);

impl Reducer for Counter {
    type State = usize;
    type Action = bool;

    fn reduce(
        &mut self,
        state: &mut usize,
        action: &bool,
        _: &mut ReduceContext<'_>,
    ) -> Effect<bool> {
        assert!(!self.0.swap(true, Ordering::SeqCst));
        if *action {
            *state += 1;
        }
        self.0.store(false, Ordering::SeqCst);
        Effect::none()
    }
}

// Failure to detach before callback would rerun the selector for reentrant ingress.
#[test]
fn one_shot_callback_is_post_commit_detached_and_can_enqueue_ingress() {
    let runtime = ThreadRuntime::new();
    let reducing = Arc::new(AtomicBool::new(false));
    let selections = Arc::new(AtomicUsize::new(0));
    let calls = Rc::new(Cell::new(0));
    let store = Store::new(0, Counter(Arc::clone(&reducing)), runtime.runtime());
    futures_executor::block_on(async {
        let observation = store
            .observe_distinct({
                let selections = Arc::clone(&selections);
                move |state| {
                    selections.fetch_add(1, Ordering::SeqCst);
                    *state
                }
            })
            .await
            .unwrap();
        assert_eq!(selections.load(Ordering::SeqCst), 1);
        let mut callback = Box::pin(observation.on_change(|value| {
            assert_eq!(value, 1);
            assert!(!reducing.load(Ordering::SeqCst));
            calls.set(calls.get() + 1);
            drop(store.try_send(true).unwrap());
        }));
        let waker = futures_util::task::noop_waker();
        let mut context = Context::from_waker(&waker);
        assert!(matches!(
            callback.as_mut().poll(&mut context),
            Poll::Pending
        ));
        store.send(false).await.unwrap().finish().await;
        assert_eq!(selections.load(Ordering::SeqCst), 2);
        assert!(matches!(
            callback.as_mut().poll(&mut context),
            Poll::Pending
        ));
        assert_eq!(calls.get(), 0);
        store.send(true).await.unwrap().finish().await;
        assert!(callback.await);
        assert_eq!(store.read(|state| *state).await.unwrap(), 2);
        assert_eq!(calls.get(), 1);
        assert_eq!(selections.load(Ordering::SeqCst), 3);
        store.shutdown().await.unwrap();
    });
}

// Callback reporting must record before its handler reenters Store ingress.
#[cfg(feature = "test-util")]
#[test]
fn observation_issue_handler_reentry_preserves_post_commit_order() {
    use std::sync::Mutex;

    use rca_core::{Issue, testing::IssueRecorder};

    let runtime = ThreadRuntime::new();
    let reducing = Arc::new(AtomicBool::new(false));
    let recorder = IssueRecorder::new();
    let reporter = recorder.reporter();
    let issue = Issue::new("observation.changed", "committed state changed");
    let events = Arc::new(Mutex::new(Vec::new()));
    let reentry = Arc::new(Mutex::new(None::<(Store<usize, bool>, IssueRecorder)>));
    let store = Store::builder(0, Counter(Arc::clone(&reducing)), runtime.runtime())
        .issue_recorder(recorder.clone())
        .with_issue_handler({
            let reducing = Arc::clone(&reducing);
            let events = Arc::clone(&events);
            // A weak holder avoids a handler-to-recorder ownership cycle.
            let reentry = Arc::downgrade(&reentry);
            move |issue| {
                assert!(!reducing.load(Ordering::SeqCst));
                let reentry = reentry.upgrade().unwrap();
                let reentry = reentry.lock().unwrap();
                let (store, recorder) = reentry.as_ref().unwrap();
                assert_eq!(recorder.snapshot().as_slice(), std::slice::from_ref(issue));
                events.lock().unwrap().extend(["recorded", "handler"]);
                drop(store.try_send(true).unwrap());
            }
        })
        .build()
        .unwrap();
    *reentry.lock().unwrap() = Some((store.clone(), recorder.clone()));
    futures_executor::block_on(async {
        let observation = store.observe(|state| *state).await.unwrap();
        let callback = observation.on_change(|value| {
            assert_eq!(value, 1);
            assert!(!reducing.load(Ordering::SeqCst));
            events.lock().unwrap().push("callback");
            reporter.report(issue.clone()).unwrap();
            events.lock().unwrap().push("report-returned");
        });
        store.send(true).await.unwrap().finish().await;
        assert!(callback.await);
        drop(reentry.lock().unwrap().take());
        assert_eq!(store.read(|state| *state).await.unwrap(), 2);
        assert_eq!(recorder.snapshot(), [issue]);
        assert_eq!(
            *events.lock().unwrap(),
            ["callback", "recorded", "handler", "report-returned"]
        );
        store.shutdown().await.unwrap();
    });
}

// Cancelling the callback future must release registration even before its first poll.
#[test]
fn dropping_unpolled_callback_prevents_later_selection_and_invocation() {
    let runtime = ThreadRuntime::new();
    let selections = Arc::new(AtomicUsize::new(0));
    let store = Store::new(
        0,
        Counter(Arc::new(AtomicBool::new(false))),
        runtime.runtime(),
    );
    futures_executor::block_on(async {
        let observation = store
            .observe({
                let selections = Arc::clone(&selections);
                move |state| {
                    selections.fetch_add(1, Ordering::SeqCst);
                    *state
                }
            })
            .await
            .unwrap();
        let callback = observation.on_change(|_| panic!("cancelled callback"));
        drop(callback);
        store.send(true).await.unwrap().finish().await;
        assert_eq!(selections.load(Ordering::SeqCst), 1);
        store.shutdown().await.unwrap();
    });
}

// Initial delivery consumed earlier must not cause on_change to skip its first update.
#[test]
fn callback_after_initial_delivery_handles_the_next_publication() {
    let runtime = ThreadRuntime::new();
    let store = Store::new(
        0,
        Counter(Arc::new(AtomicBool::new(false))),
        runtime.runtime(),
    );
    futures_executor::block_on(async {
        let mut observation = store.observe(|state| *state).await.unwrap();
        assert_eq!(observation.next().await, Some(0));
        store.send(true).await.unwrap().finish().await;
        assert!(observation.on_change(|value| assert_eq!(value, 1)).await);
        store.shutdown().await.unwrap();
    });
}

// A buffered commit does not admit an unpolled callback after Store authority ends.
#[test]
fn root_shutdown_suppresses_unpolled_callback_with_buffered_commit() {
    let runtime = ThreadRuntime::new();
    let store = Store::new(
        0,
        Counter(Arc::new(AtomicBool::new(false))),
        runtime.runtime(),
    );
    futures_executor::block_on(async {
        let observation = store.observe(|state| *state).await.unwrap();
        store.send(true).await.unwrap().finish().await;
        let callback = observation.on_change(|_| panic!("closed callback"));
        store.shutdown().await.unwrap();
        assert!(!callback.await);
    });
}
