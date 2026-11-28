//! Checks cancellation inheritance without losing root or Store ownership.

#[cfg(not(miri))]
#[path = "../src/allocation_test/child.rs"]
mod child;

#[path = "support/thread_runtime.rs"]
mod support;

use async_channel::{Receiver, Sender};
use futures_executor::block_on;
use rca_core::{Effect, ReduceContext, Reducer, Store};
use support::ThreadRuntime;

/// Records both admitted actions and any output released after cancellation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Action {
    Start,
    Child,
    Output,
    CancelSource,
    CancelLocal,
}

/// Places the inheritance boundary before or after an explicit local wrapper.
#[derive(Clone, Copy)]
enum Mode {
    Inherit,
    Transfer,
    Outer,
    Inner,
}

/// Holds a child effect until the test has cancelled the chosen identity.
struct Feature {
    mode: Mode,
    started: Sender<()>,
    release: Receiver<()>,
}
impl Reducer for Feature {
    type State = Vec<Action>;
    type Action = Action;
    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Action,
        _: &mut ReduceContext<'_>,
    ) -> Effect<Action> {
        state.push(*action);
        match action {
            Action::Start => Effect::send(Action::Child).cancellable("source"),
            Action::Child => {
                let started = self.started.clone();
                let release = self.release.clone();
                let effect = Effect::run(move |sender| async move {
                    started.send(()).await.unwrap();
                    let _ = release.recv().await;
                    let _ = sender.send(Action::Output).await;
                });
                match self.mode {
                    Mode::Inherit => effect,
                    Mode::Transfer => effect.without_inherited_cancellation(),
                    Mode::Outer => effect.without_inherited_cancellation().cancellable("local"),
                    Mode::Inner => effect.cancellable("local").without_inherited_cancellation(),
                }
            }
            Action::CancelSource => Effect::cancel("source"),
            Action::CancelLocal => Effect::cancel("local"),
            Action::Output => Effect::none(),
        }
    }
}

/// Checks the full action log, including whether cancellation suppresses output.
///
/// Bypassing the boundary loses output; erasing explicit wrappers admits it wrongly.
fn cancellation_case(mode: Mode, cancel: Action, expected_output: bool) {
    let runtime = ThreadRuntime::new();
    let (started, ready) = async_channel::bounded(1);
    let (release, gate) = async_channel::bounded(1);
    let store = Store::new(
        Vec::new(),
        Feature {
            mode,
            started,
            release: gate,
        },
        runtime.runtime(),
    );
    block_on(async {
        let root = store.send(Action::Start).await.unwrap();
        ready.recv().await.unwrap();
        assert!(!root.is_finished(), "root receipt owns pending child");
        store.send(cancel).await.unwrap().finish().await;
        let _ = release.send(()).await;
        root.finish().await;
        let expected = if expected_output {
            vec![Action::Start, Action::Child, cancel, Action::Output]
        } else {
            vec![Action::Start, Action::Child, cancel]
        };
        assert_eq!(store.read(Clone::clone).await.unwrap(), expected);
        store.shutdown().await.unwrap();
    });
    runtime.join_all();
}

#[test]
fn inherited_id_is_removed_only_by_explicit_boundary() {
    #[cfg(not(miri))]
    if child::run_in_child("inherited_id_is_removed_only_by_explicit_boundary") {
        return;
    }

    cancellation_case(Mode::Inherit, Action::CancelSource, false);
    cancellation_case(Mode::Transfer, Action::CancelSource, true);
}

#[test]
fn explicit_wrappers_survive_both_modifier_orders() {
    #[cfg(not(miri))]
    if child::run_in_child("explicit_wrappers_survive_both_modifier_orders") {
        return;
    }

    for mode in [Mode::Outer, Mode::Inner] {
        cancellation_case(mode, Action::CancelSource, true);
        cancellation_case(mode, Action::CancelLocal, false);
    }
}

// Dropping lineage or Store ownership would hang finish/shutdown while gate stays closed.
#[test]
fn transferred_work_retains_root_cancellation_and_shutdown_ownership() {
    #[cfg(not(miri))]
    if child::run_in_child("transferred_work_retains_root_cancellation_and_shutdown_ownership") {
        return;
    }

    for shutdown in [false, true] {
        let runtime = ThreadRuntime::new();
        let (started, ready) = async_channel::bounded(1);
        let (_release, gate) = async_channel::bounded(1);
        let store = Store::new(
            Vec::new(),
            Feature {
                mode: Mode::Transfer,
                started,
                release: gate,
            },
            runtime.runtime(),
        );
        block_on(async {
            let root = store.send(Action::Start).await.unwrap();
            ready.recv().await.unwrap();
            assert!(!root.is_finished());
            if shutdown {
                store.shutdown().await.unwrap();
            } else {
                root.cancel();
            }
            root.finish().await;
            assert!(root.is_finished());
            if !shutdown {
                assert!(root.is_cancelled());
                assert_eq!(
                    store.read(Clone::clone).await.unwrap(),
                    [Action::Start, Action::Child]
                );
                store.shutdown().await.unwrap();
            }
        });
        runtime.join_all();
    }
}

/// Distinguishes queued obsolete work from the output it would wrongly revive.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StaleAction {
    Start,
    Block,
    Cancel,
    Transfer,
    Output,
}
/// Blocks the reducer while another thread fixes cancellation and send order.
struct StaleFeature {
    captured: Sender<rca_core::EffectSender<StaleAction>>,
    blocked: std::sync::mpsc::Sender<()>,
    unblock: std::sync::mpsc::Receiver<()>,
}
impl Reducer for StaleFeature {
    type State = Vec<StaleAction>;
    type Action = StaleAction;
    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &StaleAction,
        _: &mut ReduceContext<'_>,
    ) -> Effect<StaleAction> {
        state.push(*action);
        match action {
            StaleAction::Start => {
                let captured = self.captured.clone();
                Effect::run(move |sender| async move {
                    captured.send(sender).await.unwrap();
                    std::future::pending::<()>().await;
                })
                .cancellable("source")
            }
            StaleAction::Block => {
                if self.blocked.send(()).is_err() {
                    return Effect::none();
                }
                // A failed ordering assertion also releases the reducer during cleanup.
                let _ = self.unblock.recv();
                Effect::none()
            }
            StaleAction::Cancel => Effect::cancel("source"),
            StaleAction::Transfer => {
                Effect::send(StaleAction::Output).without_inherited_cancellation()
            }
            StaleAction::Output => Effect::none(),
        }
    }
}

// If admission moved after boundary compilation, queued obsolete Transfer could revive Output.
#[test]
fn queued_obsolete_action_cannot_enter_transfer_reducer() {
    #[cfg(not(miri))]
    if child::run_in_child("queued_obsolete_action_cannot_enter_transfer_reducer") {
        return;
    }

    let runtime = ThreadRuntime::new();
    let (captured, sender_rx) = async_channel::bounded(1);
    let (blocked_tx, blocked_rx) = std::sync::mpsc::channel();
    let (unblock_tx, unblock_rx) = std::sync::mpsc::channel();
    let store = Store::new(
        Vec::new(),
        StaleFeature {
            captured,
            blocked: blocked_tx,
            unblock: unblock_rx,
        },
        runtime.runtime(),
    );
    block_on(async {
        let root = store.send(StaleAction::Start).await.unwrap();
        let sender = sender_rx.recv().await.unwrap();
        let ordering_store = store.clone();
        let ordering = std::thread::spawn(move || {
            blocked_rx.recv().unwrap();
            let cancel = ordering_store.try_send(StaleAction::Cancel).unwrap();
            let mut output = Box::pin(sender.send(StaleAction::Transfer));
            let pending = output
                .as_mut()
                .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
                .is_pending();
            drop(output);
            unblock_tx.send(()).unwrap();
            assert!(pending, "effect action was enqueued behind cancellation");
            cancel
        });
        store.send(StaleAction::Block).await.unwrap().finish().await;
        ordering
            .join()
            .unwrap()
            .finish()
            .await
            .unwrap()
            .finish()
            .await;
        root.finish().await;
        assert_eq!(
            store.read(Clone::clone).await.unwrap(),
            [StaleAction::Start, StaleAction::Block, StaleAction::Cancel]
        );
        store.shutdown().await.unwrap();
    });
    runtime.join_all();
}
