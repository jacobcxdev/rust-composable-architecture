//! Qualifies production actor budgets and ancestor receipt ownership.

#[expect(
    dead_code,
    reason = "manual host support is shared by independent integration targets"
)]
#[path = "support/mod.rs"]
mod support;

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Waker};

use futures_util::FutureExt;
use rca_core::{ActionTask, Effect, MonotonicInstant, Reduce, Store};

/// Polls once without introducing an executor or detached work.
fn poll<T>(future: Pin<&mut (impl Future<Output = T> + ?Sized)>) -> Poll<T> {
    future.poll(&mut Context::from_waker(Waker::noop()))
}

/// Supplies a finite flood without treating the private budget as public API.
const BACKLOG: usize = 129;

/// Distinguishes admitted work from a saturated timer that must never fire.
#[derive(Clone, Copy, Debug)]
enum BudgetAction {
    ArmTimer,
    Commit(usize),
    TimerFired,
}

// Catches removing either production budget, FIFO reordering, or premature receipts.
#[test]
fn production_ready_ingress_yields_before_exhaustion_with_and_without_timer() {
    for with_timer in [false, true] {
        let host = support::ManualRuntime::new();
        let count = Arc::new(AtomicUsize::new(0));
        let reductions = Arc::clone(&count);
        let reducer =
            Reduce::<Vec<usize>, BudgetAction, _>::new(move |state, action, _| match *action {
                BudgetAction::ArmTimer => {
                    Effect::send_at(MonotonicInstant::MAX, BudgetAction::TimerFired)
                }
                BudgetAction::Commit(index) => {
                    state.push(index);
                    reductions.fetch_add(1, Ordering::SeqCst);
                    Effect::none()
                }
                BudgetAction::TimerFired => panic!("saturated timer must not fire"),
            });
        let store = Store::builder(Vec::new(), reducer, host.runtime())
            .mailbox_capacity(BACKLOG + 1)
            .build()
            .unwrap();
        let mut actor = host.take_only_queued_task();
        let timer = with_timer.then(|| store.try_send(BudgetAction::ArmTimer).unwrap());
        let mut receipts: Vec<_> = (0..BACKLOG)
            .map(|index| store.try_send(BudgetAction::Commit(index)).unwrap())
            .collect();
        let mut last_receipt = Box::pin(receipts.pop().unwrap().finish());
        assert!(poll(last_receipt.as_mut()).is_pending());

        assert!(poll(actor.as_mut()).is_pending());
        // The external peer gets control after exactly one actor poll, before
        // exhaustion. No assertion gate or host task-per-output yield assists it.
        let before_peer = count.load(Ordering::SeqCst);
        assert!(before_peer > 0 && before_peer < BACKLOG);
        assert!(poll(last_receipt.as_mut()).is_pending());
        for _ in 0..BACKLOG {
            if count.load(Ordering::SeqCst) == BACKLOG {
                break;
            }
            assert!(poll(actor.as_mut()).is_pending());
        }
        assert_eq!(count.load(Ordering::SeqCst), BACKLOG);
        assert!(last_receipt.now_or_never().unwrap().unwrap().is_finished());
        let timer_task = timer.map(|receipt| receipt.finish().now_or_never().unwrap().unwrap());
        for receipt in receipts {
            let task = receipt.finish().now_or_never().unwrap().unwrap();
            assert!(task.is_finished());
            assert!(!task.is_cancelled());
        }
        let mut shutdown = Box::pin(store.shutdown_into_state());
        assert!(poll(shutdown.as_mut()).is_pending());
        assert!(poll(actor.as_mut()).is_ready());
        assert_eq!(
            poll(shutdown.as_mut()).map(Result::unwrap),
            Poll::Ready((0..BACKLOG).collect::<Vec<_>>())
        );
        if let Some(task) = timer_task {
            assert!(task.is_finished());
            assert!(task.is_cancelled(), "shutdown retired the active timer");
        }
    }
}

// Catches retaining budget debt after a listener really returned Pending.
#[test]
fn genuine_pending_wait_resets_timer_free_budget() {
    let host = support::ManualRuntime::new();
    let count = Arc::new(AtomicUsize::new(0));
    let reductions = Arc::clone(&count);
    let reducer = Reduce::<(), (), _>::new(move |(), (), _| {
        reductions.fetch_add(1, Ordering::SeqCst);
        Effect::none()
    });
    let store = Store::builder((), reducer, host.runtime())
        .mailbox_capacity(80)
        .build()
        .unwrap();
    let mut actor = host.take_only_queued_task();
    // Both batches are below one budget; their combined length exceeds it.
    let mut expected = 0;
    for batch in [50, 30] {
        let receipts: Vec<_> = (0..batch).map(|_| store.try_send(()).unwrap()).collect();
        assert!(poll(actor.as_mut()).is_pending());
        expected += batch;
        assert_eq!(count.load(Ordering::SeqCst), expected);
        for receipt in receipts {
            assert!(
                receipt
                    .finish()
                    .now_or_never()
                    .unwrap()
                    .unwrap()
                    .is_finished()
            );
        }
    }
    drop(store);
    assert!(poll(actor.as_mut()).is_ready());
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// Creates feedback whose ancestor receipt remains owned by its effect.
enum CycleAction {
    Start,
    Feedback,
}

// Catches falsely treating a descendant's ancestor wait as completed, losing
// cancellation wakeups, or acknowledging teardown before captured data drops.
#[test]
fn external_watchdog_cancels_passive_ancestor_cycle_without_detached_work() {
    let host = support::ManualRuntime::new();
    let (supply, ancestor) = async_channel::bounded::<ActionTask>(1);
    let payload = Arc::new(());
    let weak_payload = Arc::downgrade(&payload);
    let mut captured = Some(payload);
    let reducer = Reduce::<Vec<CycleAction>, CycleAction, _>::new(move |state, action, _| {
        state.push(*action);
        let ancestor = ancestor.clone();
        let payload = captured.take().unwrap();
        Effect::run(move |_| async move {
            #[expect(
                clippy::no_effect_underscore_binding,
                reason = "The owned capture must remain alive across the ancestor wait until cancellation"
            )]
            let _payload = payload;
            ancestor.recv().await.unwrap().finish().await;
        })
    });
    let store = Store::new(Vec::new(), reducer, host.runtime());
    let mut actor = host.take_only_queued_task();
    let receipt = store.try_send(CycleAction::Start).unwrap();
    assert!(poll(actor.as_mut()).is_pending());
    let root = receipt.finish().now_or_never().unwrap().unwrap();
    supply.try_send(root.clone()).unwrap();
    let mut descendant = host.take_only_queued_task();

    // Bounded external watchdog: poll the actual descendant, then allow actor
    // read feedback and a second descendant poll. This is actual Pending, not
    // an unpolled/starved task. The watchdog is outside the root's lineage.
    assert!(poll(descendant.as_mut()).is_pending());
    let mut read = Box::pin(store.read(Clone::clone));
    assert!(poll(read.as_mut()).is_pending());
    assert!(poll(actor.as_mut()).is_pending());
    assert_eq!(
        poll(read.as_mut()).map(Result::unwrap),
        Poll::Ready(vec![CycleAction::Start])
    );
    assert!(poll(descendant.as_mut()).is_pending());
    assert!(!root.is_finished());
    assert!(!root.is_cancelled());
    assert!(weak_payload.upgrade().is_some());

    root.cancel();
    assert!(root.is_cancelled());
    assert!(
        !root.is_finished(),
        "request alone does not destroy descendant"
    );
    // Cancellation reaches the actor through control wake, with no new input.
    assert!(poll(actor.as_mut()).is_pending());
    assert!(poll(descendant.as_mut()).is_ready());
    drop(descendant);
    assert!(
        weak_payload.upgrade().is_none(),
        "cancelled future released capture"
    );
    assert!(poll(actor.as_mut()).is_pending());
    assert!(root.is_finished());
    assert!(root.finish().now_or_never().is_some());
    drop(read);
    let closed = store.closed();
    drop(store);
    assert!(poll(actor.as_mut()).is_ready());
    assert!(closed.now_or_never().is_some());
    // The host captured exactly actor + descendant; neither was detached.
    assert_eq!(host.spawn_count(), 2);
}

// Catches confusing ordinary send commit feedback with whole-lineage completion.
#[test]
fn ordinary_effect_send_commit_feedback_does_not_wait_for_ancestor_completion() {
    let host = support::ManualRuntime::new();
    let reducer = Reduce::<Vec<CycleAction>, CycleAction, _>::new(|state, action, _| {
        state.push(*action);
        match action {
            CycleAction::Start => Effect::run(|sender| async move {
                sender.send(CycleAction::Feedback).await.unwrap();
            }),
            CycleAction::Feedback => Effect::none(),
        }
    });
    let store = Store::new(Vec::new(), reducer, host.runtime());
    let mut actor = host.take_only_queued_task();
    let receipt = store.try_send(CycleAction::Start).unwrap();
    assert!(poll(actor.as_mut()).is_pending());
    let root = receipt.finish().now_or_never().unwrap().unwrap();
    let mut descendant = host.take_only_queued_task();
    assert!(poll(descendant.as_mut()).is_pending());
    assert!(poll(actor.as_mut()).is_pending());
    assert!(poll(descendant.as_mut()).is_ready());
    drop(descendant);
    assert!(poll(actor.as_mut()).is_pending());
    assert!(root.is_finished());
    assert!(!root.is_cancelled());
    let mut shutdown = Box::pin(store.shutdown_into_state());
    assert!(poll(shutdown.as_mut()).is_pending());
    assert!(poll(actor.as_mut()).is_ready());
    assert_eq!(
        poll(shutdown.as_mut()).map(Result::unwrap),
        Poll::Ready(vec![CycleAction::Start, CycleAction::Feedback])
    );
    assert_eq!(host.spawn_count(), 2);
}
