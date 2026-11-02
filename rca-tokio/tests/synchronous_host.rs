//! Qualifies synchronous callers without blocking the executor driving their Stores.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

use rca_core::testing::IssueRecorder;
use rca_core::{Effect, Issue, ReduceContext, Reducer, Store};
use rca_tokio::TokioRuntime;

/// Carries a unique job value through a delayed effect and back to state.
#[derive(Debug, PartialEq, Eq)]
enum Action {
    Start(u8),
    Finished(u8),
}

/// Records both the delivered value and the executor thread that reduces it.
struct Jobs;

impl Reducer for Jobs {
    type State = Vec<(u8, thread::ThreadId)>;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        match action {
            Action::Start(value) => {
                Effect::send_after(Duration::from_millis(1), Action::Finished(*value))
            }
            Action::Finished(value) => {
                state.push((*value, thread::current().id()));
                Effect::none()
            }
        }
    }
}

#[test]
fn synchronous_admission_retains_full_mailbox_action_until_host_is_driven() {
    let host = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .expect("build host");
    let store = {
        let _entered = host.enter();
        Store::builder(
            Vec::new(),
            Jobs,
            TokioRuntime::current().expect("adapt host"),
        )
        .mailbox_capacity(1)
        .build()
        .expect("build Store")
    };

    // Entering the runtime permits construction; it does not drive this actor.
    let first = store.try_send(Action::Start(3)).expect("first admission");
    let rejected = store.try_send(Action::Start(7)).expect_err("mailbox full");
    assert!(rejected.is_full());
    let recovered = rejected.into_action();
    assert_eq!(recovered, Action::Start(7));

    host.block_on(async {
        tokio::time::timeout(Duration::from_secs(15), async {
            first.finish().await.expect("first commit").finish().await;
            store
                .send(recovered)
                .await
                .expect("retry commit")
                .finish()
                .await;
            let state = store.shutdown_into_state().await.expect("extract state");
            assert_eq!(
                state,
                [(3, thread::current().id()), (7, thread::current().id())]
            );
        })
        .await
        .expect("host progress watchdog");
    });
}

#[test]
fn synchronous_caller_shares_background_host_and_waits_off_executor_thread() {
    thread::scope(|scope| {
        let (publish, receive) = mpsc::sync_channel(1);
        let worker = scope.spawn(move || {
            let host = tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
                .expect("build background host");
            host.block_on(async {
                let runtime = TokioRuntime::current().expect("adapt background host");
                let first = Store::new(Vec::new(), Jobs, runtime.clone());
                let second = Store::new(Vec::new(), Jobs, runtime);
                let first_closed = first.closed();
                let second_closed = second.closed();
                publish
                    .send((host.handle().clone(), first, second, thread::current().id()))
                    .expect("publish Stores");
                // Watchdogs bound a regression; elapsed time is not a performance assertion.
                tokio::time::timeout(Duration::from_secs(30), async {
                    first_closed.await;
                    second_closed.await;
                })
                .await
                .expect("host shutdown watchdog");
            });
        });
        let (handle, first, second, executor_thread) = receive
            .recv_timeout(Duration::from_secs(15))
            .expect("receive background Stores");
        assert_ne!(executor_thread, thread::current().id());
        let first_receipt = first.try_send(Action::Start(11)).expect("first admission");
        let second_receipt = second
            .try_send(Action::Start(23))
            .expect("second admission");
        let stale = first.clone();

        // This caller is outside Tokio. The worker keeps driving its current-thread host.
        handle.block_on(async {
            tokio::time::timeout(Duration::from_secs(15), async {
                first_receipt
                    .finish()
                    .await
                    .expect("first commit")
                    .finish()
                    .await;
                second_receipt
                    .finish()
                    .await
                    .expect("second commit")
                    .finish()
                    .await;
                assert_eq!(
                    first.shutdown_into_state().await.expect("first state"),
                    [(11, executor_thread)]
                );
                assert_eq!(
                    second.shutdown_into_state().await.expect("second state"),
                    [(23, executor_thread)]
                );
            })
            .await
            .expect("cross-thread progress watchdog");
        });
        let rejected = stale
            .try_send(Action::Start(31))
            .expect_err("Store stopped");
        assert!(rejected.is_store_stopped());
        assert_eq!(rejected.into_action(), Action::Start(31));
        worker.join().expect("background host exited");
    });
}

/// Checks exact work and complete teardown with 1, 2, 4 or 8 concurrent producers.
///
/// Lost or duplicated commits, unfinished effects, stale observations and retained
/// selectors fail independently. Observation coalescing need not deliver every value.
#[expect(
    clippy::too_many_lines,
    reason = "keeps concurrent admission, delivery and complete teardown together"
)]
fn concurrent_producers_and_observers_preserve_exact_work(count: u8) {
    /// Counts release of a selector or issue-handler capture independently of its calls.
    struct CaptureDrop(Arc<AtomicUsize>);
    impl Drop for CaptureDrop {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// Requires each admitted job's immediate issue to precede its deferred issue.
    struct ReportingJobs(Arc<tokio::sync::Barrier>);
    impl Reducer for ReportingJobs {
        type State = <Jobs as Reducer>::State;
        type Action = Action;

        fn reduce(
            &mut self,
            state: &mut Self::State,
            action: &Action,
            context: &mut ReduceContext<'_>,
        ) -> Effect<Action> {
            let timer = Jobs.reduce(state, action, context);
            let Action::Start(id) = action else {
                return timer;
            };
            context
                .report_issue(Issue::new("sync", id.to_string()))
                .expect("synchronous report accepted");
            let id = *id;
            let reporter = context.issue_reporter();
            let ready = Arc::clone(&self.0);
            Effect::merge([
                timer,
                Effect::run(move |_| async move {
                    // All C producers reach each round without blocking a worker.
                    ready.wait().await;
                    reporter
                        .report(Issue::new("deferred", id.to_string()))
                        .expect("deferred report accepted");
                }),
            ])
        }
    }

    let host = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_time()
        .build()
        .expect("build concurrent host");
    host.block_on(async {
        tokio::time::timeout(Duration::from_secs(15), async {
            let total = usize::from(count) * 4;
            let report_ready = Arc::new(tokio::sync::Barrier::new(usize::from(count)));
            let recorder = IssueRecorder::new();
            let reporter = recorder.reporter();
            let handler_calls = Arc::new(AtomicUsize::new(0));
            let handler_drops = Arc::new(AtomicUsize::new(0));
            let handled: Arc<Vec<_>> = Arc::new((0..total).map(|_| AtomicUsize::new(0)).collect());
            let store = Store::builder(
                Vec::new(),
                ReportingJobs(Arc::clone(&report_ready)),
                TokioRuntime::current().expect("adapt concurrent host"),
            )
            .mailbox_capacity(1)
            .issue_recorder(recorder.clone())
            .with_issue_handler({
                let calls = Arc::clone(&handler_calls);
                let handled = Arc::clone(&handled);
                let capture = CaptureDrop(Arc::clone(&handler_drops));
                move |issue| {
                    let _keep_capture = &capture;
                    let id: usize = issue.message.parse().expect("issued work identity");
                    let before = match issue.code {
                        "sync" => 0,
                        "deferred" => 1,
                        _ => panic!("unexpected issue code"),
                    };
                    assert_eq!(
                        handled[id].compare_exchange(
                            before,
                            before + 1,
                            Ordering::SeqCst,
                            Ordering::SeqCst,
                        ),
                        Ok(before)
                    );
                    calls.fetch_add(1, Ordering::SeqCst);
                }
            })
            .build()
            .expect("bounded Store");
            let closed = store.closed();
            let start = Arc::new(tokio::sync::Barrier::new(usize::from(count) * 2 + 1));
            let observed_final = Arc::new(tokio::sync::Barrier::new(usize::from(count) + 1));
            let selector_drops = Arc::new(AtomicUsize::new(0));
            let selected = Arc::new(AtomicUsize::new(0));
            let mut consumers = tokio::task::JoinSet::new();
            for _ in 0..count {
                let capture = CaptureDrop(Arc::clone(&selector_drops));
                let calls = Arc::clone(&selected);
                let mut values = store
                    .observe(move |state| {
                        let _keep_capture = &capture;
                        calls.fetch_add(1, Ordering::SeqCst);
                        state.len()
                    })
                    .await
                    .expect("consumer registration");
                assert_eq!(values.next().await, Some(0));
                let start = Arc::clone(&start);
                let observed_final = Arc::clone(&observed_final);
                consumers.spawn(async move {
                    start.wait().await;
                    let mut last = 0;
                    while last < total {
                        let next = values.next().await.expect("consumer remains live");
                        assert!(next >= last && next <= total);
                        last = next;
                    }
                    observed_final.wait().await;
                    assert_eq!(values.next().await, None);
                    last
                });
            }
            let mut producers = tokio::task::JoinSet::new();
            for producer in 0..count {
                let store = store.clone();
                let start = Arc::clone(&start);
                producers.spawn(async move {
                    start.wait().await;
                    let mut accepted = 0;
                    let mut finished = 0;
                    for offset in 0..4 {
                        let unique_id = producer * 4 + offset;
                        let task = store
                            .send(Action::Start(unique_id))
                            .await
                            .expect("producer admission");
                        accepted += 1;
                        task.finish().await;
                        assert!(task.is_finished());
                        assert!(!task.is_cancelled());
                        finished += 1;
                    }
                    (accepted, finished)
                });
            }
            start.wait().await;
            let (mut accepted, mut finished) = (0, 0);
            while let Some(result) = producers.join_next().await {
                let counts = result.expect("producer exits");
                accepted += counts.0;
                finished += counts.1;
            }
            assert_eq!((accepted, finished), (total, total));
            observed_final.wait().await;
            let state = store
                .shutdown_into_state()
                .await
                .expect("extract final state");
            closed.await;
            let mut delivered = 0;
            while let Some(result) = consumers.join_next().await {
                assert_eq!(result.expect("consumer exits"), total);
                delivered += 1;
            }
            assert_eq!(delivered, usize::from(count));
            assert_eq!(
                selected.load(Ordering::SeqCst),
                usize::from(count) * (2 * total + 1)
            );
            assert_eq!(selector_drops.load(Ordering::SeqCst), usize::from(count));
            assert_eq!(Arc::strong_count(&selector_drops), 1);
            let mut ids: Vec<_> = state.into_iter().map(|(id, _)| id).collect();
            for producer in 0..count {
                let ordered: Vec<_> = ids
                    .iter()
                    .copied()
                    .filter(|id| id / 4 == producer)
                    .collect();
                assert_eq!(
                    ordered,
                    (producer * 4..producer * 4 + 4).collect::<Vec<_>>()
                );
            }
            ids.sort_unstable();
            assert_eq!(ids, (0..count * 4).collect::<Vec<_>>());
            let issues = recorder.snapshot();
            assert_eq!(issues.len(), 2 * total);
            for id in 0..total {
                let codes: Vec<_> = issues
                    .iter()
                    .filter(|issue| issue.message == id.to_string())
                    .map(|issue| issue.code)
                    .collect();
                assert_eq!(codes, ["sync", "deferred"]);
                assert_eq!(handled[id].load(Ordering::SeqCst), 2);
            }
            assert_eq!(handler_calls.load(Ordering::SeqCst), 2 * total);
            let late = Issue::new("late", "retained after closure");
            let location = late.location;
            let rejected = reporter.report(late).expect_err("reporting sealed");
            assert!(rejected.is_closed());
            let recovered = rejected.into_issue();
            assert_eq!(recovered.code, "late");
            assert_eq!(recovered.message, "retained after closure");
            assert_eq!(recovered.location, location);
            assert_eq!(recorder.snapshot(), issues);
            assert_eq!(handler_calls.load(Ordering::SeqCst), 2 * total);
            drop(reporter);
            drop(recorder);
            assert_eq!(handler_drops.load(Ordering::SeqCst), 1);
            assert_eq!(Arc::strong_count(&handler_drops), 1);
            assert_eq!(Arc::strong_count(&handled), 1);
            assert_eq!(Arc::strong_count(&handler_calls), 1);
            assert_eq!(Arc::strong_count(&report_ready), 1);
        })
        .await
        .expect("concurrent progress watchdog");
    });
}

#[test]
fn concurrent_producers_and_observers_preserve_exact_work_c1() {
    concurrent_producers_and_observers_preserve_exact_work(1);
}

#[test]
fn concurrent_producers_and_observers_preserve_exact_work_c2() {
    concurrent_producers_and_observers_preserve_exact_work(2);
}

#[test]
fn concurrent_producers_and_observers_preserve_exact_work_c4() {
    concurrent_producers_and_observers_preserve_exact_work(4);
}

#[test]
fn concurrent_producers_and_observers_preserve_exact_work_c8() {
    concurrent_producers_and_observers_preserve_exact_work(8);
}
