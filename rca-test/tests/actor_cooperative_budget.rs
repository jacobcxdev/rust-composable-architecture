//! Checks actor cooperation with ready peers and other roots using bounded virtual work.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures_util::FutureExt;
use rca_core::{Effect, MonotonicInstant, Reduce, Store};
use rca_test::DeterministicRuntime;

/// A finite backlog exceeding the current private batch size, not a public limit.
const BACKLOG: usize = 129;

/// Adds visible FIFO work and optionally keeps a future timer registered.
#[derive(Clone, Copy, Debug)]
enum Action {
    ArmTimer,
    Increment(usize),
    TimerFired,
}

/// Counts commits before a known ready peer, then verifies every receipt and shutdown.
fn reductions_before_ready_peer(with_timer: bool) -> usize {
    let host = DeterministicRuntime::new();
    let reductions = Arc::new(AtomicUsize::new(0));
    let reducer_reductions = Arc::clone(&reductions);
    let reducer = Reduce::<Vec<usize>, Action, _>::new(move |state, action, _| match action {
        Action::ArmTimer => Effect::send_at(MonotonicInstant::MAX, Action::TimerFired),
        Action::Increment(index) => {
            state.push(*index);
            reducer_reductions.fetch_add(1, Ordering::SeqCst);
            Effect::none()
        }
        Action::TimerFired => panic!("MAX timer must not fire"),
    });
    let store = Store::builder(Vec::new(), reducer, host.runtime())
        .mailbox_capacity(BACKLOG + 1)
        .build()
        .expect("finite mailbox");

    if with_timer {
        drop(store.try_send(Action::ArmTimer).expect("timer admitted"));
    }
    let receipts: Vec<_> = (0..BACKLOG)
        .map(|index| {
            store
                .try_send(Action::Increment(index))
                .expect("action admitted")
        })
        .collect();

    // Store construction queued the actor first. The scheduler snapshots ready
    // tasks in ID order, so this peer runs after the actor returns from one poll.
    let observed = Arc::new(AtomicUsize::new(usize::MAX));
    let peer_observed = Arc::clone(&observed);
    let peer_reductions = Arc::clone(&reductions);
    host.spawn_task(Box::pin(async move {
        peer_observed.store(peer_reductions.load(Ordering::SeqCst), Ordering::SeqCst);
    }));
    host.run_until_stalled().expect("finite admitted work");

    let before_peer = observed.load(Ordering::SeqCst);
    assert_ne!(before_peer, usize::MAX, "ready peer ran");
    assert_eq!(reductions.load(Ordering::SeqCst), BACKLOG);
    for receipt in receipts {
        let task = receipt
            .finish()
            .now_or_never()
            .expect("receipt ready after bounded drive")
            .expect("committed receipt");
        assert!(
            task.is_finished(),
            "effect-free receipt has no pending descendants"
        );
        assert!(!task.is_cancelled());
    }
    let state = futures_executor::block_on(host.run(store.shutdown_into_state()))
        .expect("shutdown drive")
        .expect("owned state");
    assert_eq!(
        state,
        (0..BACKLOG).collect::<Vec<_>>(),
        "all admitted actions committed exactly once in FIFO order"
    );
    before_peer
}

#[test]
fn timer_present_actor_yields_before_draining_ready_backlog() {
    let observed = reductions_before_ready_peer(true);
    assert!(observed > 0, "actor ran before peer");
    assert!(
        observed < BACKLOG,
        "timer-present actor drained {observed} actions before the ready peer"
    );
}

#[test]
fn timer_free_actor_yields_before_draining_ready_backlog() {
    let observed = reductions_before_ready_peer(false);
    assert!(observed > 0, "actor ran before peer");
    assert!(
        observed < BACKLOG,
        "timer-free actor drained {observed} actions before the ready peer"
    );
}

#[test]
fn pending_managed_work_does_not_block_another_root_on_the_same_runtime() {
    let host = DeterministicRuntime::new();
    let (release, gate) = async_channel::bounded::<()>(1);
    let pending_polls = Arc::new(AtomicUsize::new(0));
    let completions = Arc::new(AtomicUsize::new(0));
    let reducer_pending = Arc::clone(&pending_polls);
    let reducer_completions = Arc::clone(&completions);
    let first = Store::new(
        0,
        Reduce::<usize, (), _>::new(move |state, (), _| {
            *state += 1;
            let gate = gate.clone();
            let pending_polls = Arc::clone(&reducer_pending);
            let completions = Arc::clone(&reducer_completions);
            Effect::run(move |_| async move {
                let wait = gate.recv();
                futures_util::pin_mut!(wait);
                std::future::poll_fn(|context| {
                    let result = std::future::Future::poll(wait.as_mut(), context);
                    if result.is_pending() {
                        pending_polls.fetch_add(1, Ordering::SeqCst);
                    }
                    result
                })
                .await
                .expect("controlled release");
                completions.fetch_add(1, Ordering::SeqCst);
            })
        }),
        host.runtime(),
    );
    let second = Store::new(
        10,
        Reduce::<usize, (), _>::new(|state, (), _| {
            *state += 1;
            Effect::none()
        }),
        host.runtime(),
    );

    let first_receipt = first.try_send(()).expect("first root admitted");
    host.run_until_stalled()
        .expect("first root reached its wait");
    let first_task = first_receipt
        .finish()
        .now_or_never()
        .expect("first receipt ready")
        .expect("first root committed");
    assert!(pending_polls.load(Ordering::SeqCst) > 0);
    assert_eq!(completions.load(Ordering::SeqCst), 0);
    assert!(!first_task.is_finished());

    let second_receipt = second.try_send(()).expect("second root admitted");
    host.run_until_stalled().expect("second root progressed");
    let second_task = second_receipt
        .finish()
        .now_or_never()
        .expect("second receipt ready before releasing first root")
        .expect("second root committed");
    assert!(second_task.is_finished());
    assert!(!second_task.is_cancelled());
    assert_eq!(
        futures_executor::block_on(host.run(second.read(|state| *state)))
            .expect("second root read progressed")
            .expect("second root state"),
        11
    );
    assert_eq!(completions.load(Ordering::SeqCst), 0);
    assert!(!first_task.is_finished());

    release.try_send(()).expect("release first root");
    futures_executor::block_on(host.run(first_task.finish())).expect("first root finished");
    assert!(first_task.is_finished());
    assert!(!first_task.is_cancelled());
    assert_eq!(completions.load(Ordering::SeqCst), 1);
    assert_eq!(
        futures_executor::block_on(host.run(first.shutdown_into_state()))
            .expect("first shutdown progressed")
            .expect("first owned state"),
        1
    );
    assert_eq!(
        futures_executor::block_on(host.run(second.shutdown_into_state()))
            .expect("second shutdown progressed")
            .expect("second owned state"),
        11
    );
}
