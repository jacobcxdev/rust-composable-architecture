//! Checks mapped effect output, cancellation and shared mapper ownership against literal traces.

use rca_core::{Effect, EffectSender, ReduceContext, Reducer};
use rca_test::TestStore;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

/// Starts one mapped send and records its numeric output.
#[derive(Debug, PartialEq)]
enum Action {
    Start,
    Number(u64),
}

/// Converts an owned number into an action without another asynchronous task.
struct Feature;

impl Reducer for Feature {
    type State = Vec<u64>;
    type Action = Action;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        match action {
            Action::Start => Effect::send(42_u64).map(Action::Number),
            Action::Number(value) => {
                state.push(*value);
                Effect::none()
            }
        }
    }
}

#[test]
fn send_effect_maps_owned_output() {
    futures_executor::block_on(async {
        let mut store = TestStore::new(Vec::new(), Feature);
        let _ = store.send(Action::Start).await.expect("start commit");
        let _ = store
            .receive_with(
                |action| matches!(action, Action::Number(42)),
                |state| state.push(42),
            )
            .await
            .expect("mapped number commit");
        store
            .assert_state(|state| assert_eq!(state, &[42]))
            .await
            .expect("mapped state assertion");
        store.finish().await.expect("clean finish");
    });
}

/// Names effect outputs and explicit controls for timers, runs and replacement work.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum ChildAction {
    Start,
    Immediate,
    Delayed,
    Tick(usize),
    First,
    Second,
    Run,
    StartOld,
    StartReplacement,
    ReplacementOutput(u8),
    CancelTimer,
    CancelRun,
}

/// Cancels only the repeating timer branch.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct TimerIdentity;

/// Cancels the separately gated run branch.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct RunIdentity;

/// Links old work and its replacement without cancelling unrelated branches.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct ReplacementIdentity;

/// Builds the same finite-control scenario for both unmapped and mapped consumers.
fn child_effect(
    run_gate: async_channel::Receiver<()>,
    tick_calls: Arc<AtomicUsize>,
) -> Effect<ChildAction> {
    Effect::merge([
        Effect::send(ChildAction::Immediate),
        Effect::send_after(Duration::from_secs(1), ChildAction::Delayed),
        Effect::interval(Duration::from_secs(1), move || {
            ChildAction::Tick(tick_calls.fetch_add(1, Ordering::AcqRel))
        })
        .cancellable(TimerIdentity),
        Effect::concatenate([
            Effect::send(ChildAction::First),
            Effect::send(ChildAction::Second),
        ]),
        Effect::run(move |sender| async move {
            sender
                .send(ChildAction::Run)
                .await
                .expect("run output commit");
            let _ = run_gate.recv().await;
        })
        .cancellable(RunIdentity),
    ])
}

/// Runs the control effect without action mapping.
struct ChildFeature {
    /// Keeps the run branch suspended until the test cancels it.
    run_gate: async_channel::Receiver<()>,
    /// Counts actual interval callback calls independently of reduced output.
    tick_calls: Arc<AtomicUsize>,
    /// Returns the original sender once for replacement-authority checks.
    old_sender_capture: Option<async_channel::Sender<EffectSender<ChildAction>>>,
    /// Keeps historical work suspended until replacement cancels it.
    old_gate: async_channel::Receiver<()>,
}

impl Reducer for ChildFeature {
    type State = Vec<ChildAction>;
    type Action = ChildAction;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        match action {
            ChildAction::Start => child_effect(self.run_gate.clone(), Arc::clone(&self.tick_calls)),
            ChildAction::StartOld => {
                let capture = self.old_sender_capture.take().expect("old sender capture");
                let old_gate = self.old_gate.clone();
                Effect::run(move |sender| async move {
                    capture.send(sender).await.expect("capture old sender");
                    let _ = old_gate.recv().await;
                })
                .cancellable(ReplacementIdentity)
            }
            ChildAction::StartReplacement => Effect::send(ChildAction::ReplacementOutput(2))
                .cancellable_in_flight(ReplacementIdentity),
            ChildAction::CancelTimer => Effect::cancel(TimerIdentity),
            ChildAction::CancelRun => Effect::cancel(RunIdentity),
            action => {
                state.push(*action);
                Effect::none()
            }
        }
    }
}

/// Wraps child output while keeping cancellation commands at the parent boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ParentAction {
    Start,
    Child(ChildAction),
    CancelTimer,
    CancelRun,
}

/// Runs the same control branches with an additional action-mapping boundary.
struct ParentFeature {
    /// Keeps the mapped run branch pending until explicit cancellation.
    run_gate: async_channel::Receiver<()>,
    /// Counts the mapped interval's actual callback calls.
    tick_calls: Arc<AtomicUsize>,
    /// Exposes the original child-typed sender before parent action mapping.
    old_sender_capture: Option<async_channel::Sender<EffectSender<ChildAction>>>,
    /// Keeps the original mapped run alive until replacement.
    old_gate: async_channel::Receiver<()>,
}

impl Reducer for ParentFeature {
    type State = Vec<ChildAction>;
    type Action = ParentAction;

    fn reduce(
        &mut self,
        state: &mut Self::State,
        action: &Self::Action,
        _context: &mut ReduceContext<'_>,
    ) -> Effect<Self::Action> {
        match action {
            ParentAction::Start => {
                child_effect(self.run_gate.clone(), Arc::clone(&self.tick_calls))
                    .map(ParentAction::Child)
            }
            ParentAction::Child(ChildAction::StartOld) => {
                let capture = self.old_sender_capture.take().expect("old sender capture");
                let old_gate = self.old_gate.clone();
                Effect::run(move |sender| async move {
                    capture.send(sender).await.expect("capture old sender");
                    let _ = old_gate.recv().await;
                })
                .cancellable(ReplacementIdentity)
                .map(ParentAction::Child)
            }
            ParentAction::Child(ChildAction::StartReplacement) => {
                Effect::send(ChildAction::ReplacementOutput(2))
                    .cancellable_in_flight(ReplacementIdentity)
                    .map(ParentAction::Child)
            }
            ParentAction::CancelTimer => {
                Effect::<ChildAction>::cancel(TimerIdentity).map(ParentAction::Child)
            }
            ParentAction::CancelRun => {
                Effect::<ChildAction>::cancel(RunIdentity).map(ParentAction::Child)
            }
            ParentAction::Child(action) => {
                state.push(*action);
                Effect::none()
            }
        }
    }
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "the control and mapped sequences stay adjacent for comparison"
)]
fn mapped_effects_preserve_control_order_timers_cancellation_and_lineage() {
    let (_control_release, control_gate) = async_channel::bounded(1);
    let (_parent_release, parent_gate) = async_channel::bounded(1);
    let (_control_old_release, control_old_gate) = async_channel::bounded(1);
    let (_parent_old_release, parent_old_gate) = async_channel::bounded(1);
    let control_tick_calls = Arc::new(AtomicUsize::new(0));
    let parent_tick_calls = Arc::new(AtomicUsize::new(0));
    let mut control = TestStore::new(
        Vec::new(),
        ChildFeature {
            run_gate: control_gate,
            tick_calls: Arc::clone(&control_tick_calls),
            old_sender_capture: None,
            old_gate: control_old_gate,
        },
    );
    let mut parent = TestStore::new(
        Vec::new(),
        ParentFeature {
            run_gate: parent_gate,
            tick_calls: Arc::clone(&parent_tick_calls),
            old_sender_capture: None,
            old_gate: parent_old_gate,
        },
    );

    futures_executor::block_on(async {
        let control_task = control
            .send(ChildAction::Start)
            .await
            .expect("control start commit");
        let parent_task = parent
            .send(ParentAction::Start)
            .await
            .expect("parent start commit");

        for expected in [
            ChildAction::Immediate,
            ChildAction::First,
            ChildAction::Run,
            ChildAction::Second,
        ] {
            let _ = control
                .receive_with(
                    move |action| *action == expected,
                    move |state| state.push(expected),
                )
                .await
                .expect("control immediate output");
            let _ = parent
                .receive_with(
                    move |action| *action == ParentAction::Child(expected),
                    move |state| state.push(expected),
                )
                .await
                .expect("parent mapped immediate output");
        }

        let _ = control
            .send(ChildAction::CancelRun)
            .await
            .expect("control run cancellation");
        let _ = parent
            .send(ParentAction::CancelRun)
            .await
            .expect("parent mapped run cancellation");
        assert!(!control_task.is_finished());
        assert!(!control_task.is_cancelled());
        assert!(!parent_task.is_finished());
        assert!(!parent_task.is_cancelled());

        control
            .advance(Duration::from_secs(3))
            .await
            .expect("control timer deadline");
        parent
            .advance(Duration::from_secs(3))
            .await
            .expect("parent timer deadline");
        assert_eq!(control.now(), parent.now());

        for expected in [
            ChildAction::Delayed,
            ChildAction::Tick(0),
            ChildAction::Tick(1),
            ChildAction::Tick(2),
        ] {
            let _ = control
                .receive_with(
                    move |action| *action == expected,
                    move |state| state.push(expected),
                )
                .await
                .expect("control timer output");
            let _ = parent
                .receive_with(
                    move |action| *action == ParentAction::Child(expected),
                    move |state| state.push(expected),
                )
                .await
                .expect("parent mapped timer output");
        }
        assert_eq!(control_tick_calls.load(Ordering::Acquire), 3);
        assert_eq!(parent_tick_calls.load(Ordering::Acquire), 3);

        let _ = control
            .send(ChildAction::CancelTimer)
            .await
            .expect("control timer cancellation");
        let _ = parent
            .send(ParentAction::CancelTimer)
            .await
            .expect("parent mapped timer cancellation");
        assert!(control_task.is_finished());
        assert!(!control_task.is_cancelled());
        assert!(parent_task.is_finished());
        assert!(!parent_task.is_cancelled());

        let expected = [
            ChildAction::Immediate,
            ChildAction::First,
            ChildAction::Run,
            ChildAction::Second,
            ChildAction::Delayed,
            ChildAction::Tick(0),
            ChildAction::Tick(1),
            ChildAction::Tick(2),
        ];
        control
            .assert_state_eq(expected.to_vec())
            .await
            .expect("control output state");
        parent
            .assert_state_eq(expected.to_vec())
            .await
            .expect("parent mapped output state");
        control.finish().await.expect("control clean finish");
        parent.finish().await.expect("parent clean finish");
    });
}

#[test]
#[expect(
    clippy::too_many_lines,
    reason = "one partial-cancellation ownership trace keeps its checkpoints adjacent"
)]
fn mapped_merge_keeps_shared_mapper_alive_after_one_child_is_cancelled() {
    use futures_util::FutureExt;
    use rca_core::Reduce;
    use rca_test::{GateDropped, controlled_gate};

    /// Starts both branches, cancels the left branch or records one mapped value.
    #[derive(Debug, PartialEq)]
    enum Action {
        Start,
        CancelLeft,
        Value(u64),
    }

    /// Separates the two cancellation identities inside one mapped merge.
    #[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
    enum Branch {
        Left,
        Right,
    }

    /// Counts capture destruction independently of task completion and mapper calls.
    struct DropCount(Arc<AtomicUsize>);

    impl Drop for DropCount {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    futures_executor::block_on(async {
        let (left_release, left_gate) = controlled_gate();
        let (right_release, right_gate) = controlled_gate();
        let left_drops = Arc::new(AtomicUsize::new(0));
        let right_drops = Arc::new(AtomicUsize::new(0));
        let mapper_drops = Arc::new(AtomicUsize::new(0));
        let calls = Arc::new(AtomicUsize::new(0));
        let left_capture = DropCount(Arc::clone(&left_drops));
        let right_capture = DropCount(Arc::clone(&right_drops));
        let mapper_capture = DropCount(Arc::clone(&mapper_drops));
        let mapped_calls = Arc::clone(&calls);
        let left = Effect::future(async move {
            let _capture = left_capture;
            left_gate
                .await
                .expect("left gate is held until cancellation");
            Some(11_u64)
        })
        .cancellable(Branch::Left);
        let right = Effect::future(async move {
            let _capture = right_capture;
            right_gate.await.expect("right gate is released");
            Some(22_u64)
        })
        .cancellable(Branch::Right);
        let mut effect = Some(Effect::merge([left, right]).map(move |value| {
            let _ = &mapper_capture;
            mapped_calls.fetch_add(1, Ordering::SeqCst);
            Action::Value(value)
        }));
        let reducer = Reduce::new(
            move |state: &mut Vec<u64>, action: &Action, _| match action {
                Action::Start => effect.take().expect("start once"),
                Action::CancelLeft => Effect::cancel(Branch::Left),
                Action::Value(value) => {
                    state.push(*value);
                    Effect::none()
                }
            },
        );
        let mut store = TestStore::new(Vec::new(), reducer);
        let parent = store
            .send(Action::Start)
            .await
            .expect("admit both children");
        let blocked = parent.finish().await.expect_err("both gates are pending");
        assert!(blocked.drive_error().is_none());
        assert_eq!(
            left_release.wait_until_waiting().now_or_never(),
            Some(Ok(()))
        );
        assert_eq!(
            right_release.wait_until_waiting().now_or_never(),
            Some(Ok(()))
        );
        assert_eq!(left_drops.load(Ordering::SeqCst), 0);
        assert_eq!(right_drops.load(Ordering::SeqCst), 0);
        assert_eq!(mapper_drops.load(Ordering::SeqCst), 0);
        assert_eq!(calls.load(Ordering::SeqCst), 0);

        let cancellation = store.send(Action::CancelLeft).await.expect("cancel left");
        cancellation
            .finish()
            .await
            .expect("left cancellation completes");
        let blocked = parent.finish().await.expect_err("right remains gated");
        assert!(blocked.drive_error().is_none());
        assert_eq!(left_release.release(), Err(GateDropped));
        assert_eq!(left_drops.load(Ordering::SeqCst), 1);
        assert_eq!(right_drops.load(Ordering::SeqCst), 0);
        assert_eq!(mapper_drops.load(Ordering::SeqCst), 0);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(!parent.is_finished());
        assert!(!parent.is_cancelled());

        right_release
            .release()
            .expect("right survives left cancellation");
        let blocked = parent
            .finish()
            .await
            .expect_err("mapped output awaits assertion");
        assert!(blocked.drive_error().is_none());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(!parent.is_finished());
        let _ = store
            .receive_with(
                |action| *action == Action::Value(22),
                |state| state.push(22),
            )
            .await
            .expect("exactly one mapped right output");
        parent.finish().await.expect("parent causal work completes");
        assert!(parent.is_finished());
        assert!(!parent.is_cancelled());
        assert_eq!(right_drops.load(Ordering::SeqCst), 1);
        assert_eq!(mapper_drops.load(Ordering::SeqCst), 1);
        store
            .assert_state_eq(vec![22])
            .await
            .expect("only right committed");
        store
            .finish()
            .await
            .expect("no outputs or owned work remain");
        drop(cancellation);
        drop(parent);
        drop(store);
        assert_eq!(left_drops.load(Ordering::SeqCst), 1);
        assert_eq!(right_drops.load(Ordering::SeqCst), 1);
        assert_eq!(mapper_drops.load(Ordering::SeqCst), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    });
}

#[test]
fn mapped_cancellable_in_flight_rejects_old_sender_and_finishes_replacement() {
    let (control_capture, control_captured) = async_channel::bounded(1);
    let (parent_capture, parent_captured) = async_channel::bounded(1);
    let (_control_release, control_gate) = async_channel::bounded(1);
    let (_parent_release, parent_gate) = async_channel::bounded(1);
    let (_control_old_release, control_old_gate) = async_channel::bounded(1);
    let (_parent_old_release, parent_old_gate) = async_channel::bounded(1);
    let mut control = TestStore::new(
        Vec::new(),
        ChildFeature {
            run_gate: control_gate,
            tick_calls: Arc::new(AtomicUsize::new(0)),
            old_sender_capture: Some(control_capture),
            old_gate: control_old_gate,
        },
    );
    let mut parent = TestStore::new(
        Vec::new(),
        ParentFeature {
            run_gate: parent_gate,
            tick_calls: Arc::new(AtomicUsize::new(0)),
            old_sender_capture: Some(parent_capture),
            old_gate: parent_old_gate,
        },
    );

    futures_executor::block_on(async {
        let control_old = control
            .send(ChildAction::StartOld)
            .await
            .expect("control old start");
        let parent_old = parent
            .send(ParentAction::Child(ChildAction::StartOld))
            .await
            .expect("parent old start");
        let control_old_sender = control_captured.recv().await.expect("control old sender");
        let parent_old_sender = parent_captured.recv().await.expect("parent old sender");

        let control_replacement = control
            .send(ChildAction::StartReplacement)
            .await
            .expect("control replacement start");
        let parent_replacement = parent
            .send(ParentAction::Child(ChildAction::StartReplacement))
            .await
            .expect("parent replacement start");

        assert!(
            control_old_sender
                .send(ChildAction::ReplacementOutput(1))
                .await
                .expect_err("control old sender rejected after replacement")
                .is_cancelled()
        );
        assert!(
            parent_old_sender
                .send(ChildAction::ReplacementOutput(1))
                .await
                .expect_err("parent old sender rejected after replacement")
                .is_cancelled()
        );

        let _ = control
            .receive_with(
                |action| *action == ChildAction::ReplacementOutput(2),
                |state| state.push(ChildAction::ReplacementOutput(2)),
            )
            .await
            .expect("control replacement output");
        let _ = parent
            .receive_with(
                |action| *action == ParentAction::Child(ChildAction::ReplacementOutput(2)),
                |state| state.push(ChildAction::ReplacementOutput(2)),
            )
            .await
            .expect("parent replacement output");
        control_old
            .finish()
            .await
            .expect("control old cancellation cleanup");
        parent_old
            .finish()
            .await
            .expect("parent old cancellation cleanup");
        control_replacement
            .finish()
            .await
            .expect("control replacement lineage");
        parent_replacement
            .finish()
            .await
            .expect("parent replacement lineage");
        assert!(!control_replacement.is_cancelled());
        assert!(!parent_replacement.is_cancelled());
        control
            .assert_state_eq(vec![ChildAction::ReplacementOutput(2)])
            .await
            .expect("control replacement state");
        parent
            .assert_state_eq(vec![ChildAction::ReplacementOutput(2)])
            .await
            .expect("parent replacement state");
        control.finish().await.expect("control clean finish");
        parent.finish().await.expect("parent clean finish");
    });
}
